//! One live helper job. Only public scope and outcomes leave this module; the private
//! Desktop offer is moved directly from local IPC to the existing pinned TLS stream.
use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use xshell_protocol::pairing_access::enrollment::*;

struct Session {
    proposal: ControllerDescriptor,
    server: ServerDescriptor,
    config: Arc<rustls::ClientConfig>,
    deadline: Instant,
    sequence: AtomicU64,
    control: Mutex<Option<tls::ClientStream>>,
}
impl Session {
    fn exchange(&self, command: EnrollmentCommand) -> Result<EnrollmentResponse, AccessError> {
        if Instant::now() >= self.deadline || now() >= self.proposal.expires_at {
            return Err(AccessError::Expired);
        }
        let request = EnrollmentRequest {
            task: self.proposal.task,
            request: uuid::Uuid::new_v4(),
            sequence: self.sequence.fetch_add(1, Ordering::AcqRel) + 1,
            command,
        };
        if matches!(request.command, EnrollmentCommand::Enroll { .. }) {
            let mut stream = tls::connect(
                self.server.endpoint,
                self.config.clone(),
                &self.server.certificate,
            )?;
            return self.send(&mut stream, &request);
        }
        // Control never queues behind offer acquisition or the start's network exchange.
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if control.is_none() {
            *control = Some(tls::connect(
                self.server.endpoint,
                self.config.clone(),
                &self.server.certificate,
            )?);
        }
        let result = self.send(control.as_mut().unwrap(), &request);
        if result.is_err() {
            *control = None;
        }
        result
    }
    fn send(
        &self,
        stream: &mut tls::ClientStream,
        request: &EnrollmentRequest,
    ) -> Result<EnrollmentResponse, AccessError> {
        stream
            .sock
            .deadline(self.deadline.min(Instant::now() + tls::WRITE));
        write_frame(stream, request)?;
        stream
            .sock
            .deadline(self.deadline.min(Instant::now() + tls::READ + tls::WRITE));
        read_frame(stream)
    }
}
pub struct Job {
    operation: PairOperationId,
    session: Arc<Session>,
    path: PathBuf,
    cancelled: Arc<AtomicBool>,
    start_finished: Arc<AtomicBool>,
    response: Arc<Mutex<EnrollmentResponse>>,
}
impl Job {
    pub fn start(
        controller: &Controller,
        path: PathBuf,
        operation: PairOperationId,
    ) -> Result<Self, AccessError> {
        operation.validate()?;
        if !controller.verified
            || controller.proposal.scope.purpose != ReachabilityPurpose::MobileEnrollment
        {
            return Err(AccessError::Unauthorized);
        }
        let server = controller
            .server
            .as_ref()
            .ok_or(AccessError::BootstrapUnverified)?
            .clone();
        server.validate_for(
            &controller.proposal,
            controller.controller_fingerprint(),
            now(),
        )?;
        let recipient = server.recipient.clone().ok_or(AccessError::ScopeMismatch)?;
        let parameters = controller
            .proposal
            .scope
            .enrollment
            .as_ref()
            .ok_or(AccessError::ScopeMismatch)?
            .parameters(
                &controller.proposal.scope,
                recipient,
                controller.proposal.expires_at,
            );
        parameters.validate(now())?;
        let session = Arc::new(Session {
            proposal: controller.proposal.clone(),
            config: tls::client_config(&controller.identity, &server.certificate)?,
            server,
            deadline: controller.deadline,
            sequence: AtomicU64::new(0),
            control: Mutex::new(None),
        });
        let response = Arc::new(Mutex::new(EnrollmentResponse {
            progress: Some(EnrollmentProgress {
                operation,
                stage: EnrollmentStage::HelperReserved,
            }),
            receipt: None,
            error: None,
        }));
        let cancelled = Arc::new(AtomicBool::new(false));
        let start_finished = Arc::new(AtomicBool::new(false));
        let finished = start_finished.clone();
        let (worker, result, cancel, socket) = (
            session.clone(),
            response.clone(),
            cancelled.clone(),
            path.clone(),
        );
        std::thread::Builder::new()
            .name("pairing-enrollment".into())
            .spawn(move || {
                let work = || {
                    if cancel.load(Ordering::Acquire) {
                        return Err(AccessError::Unauthorized);
                    }
                    let start = match local::exchange(
                        &socket,
                        &DesktopRequest::Offer {
                            task: worker.proposal.task,
                            operation,
                            parameters: Box::new(parameters),
                        },
                    )? {
                        DesktopResponse::Offer { start } => start,
                        DesktopResponse::Error { error } => return Err(error),
                        _ => return Err(AccessError::InvalidRequest),
                    };
                    if cancel.load(Ordering::Acquire) || Instant::now() >= worker.deadline {
                        return Err(AccessError::Expired);
                    }
                    let mut observed = result.lock().unwrap_or_else(|e| e.into_inner());
                    if observed.receipt.is_none() {
                        observed.progress = Some(EnrollmentProgress {
                            operation,
                            stage: EnrollmentStage::DesktopOfferReady,
                        });
                    }
                    drop(observed);
                    worker.exchange(EnrollmentCommand::Enroll {
                        start: Box::new(start),
                    })
                };
                let outcome = work().unwrap_or_else(EnrollmentResponse::error);
                let mut observed = result.lock().unwrap_or_else(|e| e.into_inner());
                if observed
                    .receipt
                    .as_ref()
                    .is_none_or(|r| matches!(r.evidence, EnrollmentEvidence::OutcomeUnknown))
                {
                    *observed = outcome;
                } else if outcome.progress.is_some() {
                    observed.progress = outcome.progress;
                }
                drop(observed);
                // A lost reply is uncertain, not proof that Mobile rejected the start.
                // Publish completion only after recording the outcome, so status can
                // reconcile the exact operation without overtaking an in-flight start.
                finished.store(true, Ordering::Release);
                if cancel.load(Ordering::Acquire) {
                    let _ = local::exchange(
                        &socket,
                        &DesktopRequest::Cancel {
                            task: worker.proposal.task,
                            operation,
                        },
                    );
                }
            })
            .map_err(|_| AccessError::CoordinatorUnavailable)?;
        Ok(Self {
            operation,
            session,
            path,
            cancelled,
            start_finished,
            response,
        })
    }
    pub fn status(
        &self,
        operation: PairOperationId,
        cancel: bool,
    ) -> Result<EnrollmentResponse, AccessError> {
        if operation != self.operation {
            return Err(AccessError::Unauthorized);
        }
        if Instant::now() >= self.session.deadline {
            return Err(AccessError::Expired);
        }
        if cancel {
            self.cancelled.store(true, Ordering::Release);
        }
        let command = if cancel {
            DesktopRequest::Cancel {
                task: self.session.proposal.task,
                operation,
            }
        } else {
            DesktopRequest::Status {
                task: self.session.proposal.task,
                operation,
            }
        };
        let desktop = local::exchange(&self.path, &command).and_then(|r| match r {
            DesktopResponse::Operation { response } => Ok(response),
            DesktopResponse::Error { error } => Err(error),
            _ => Err(AccessError::InvalidRequest),
        });
        // A status may not overtake the nonrepeatable start's sequence on another
        // connection. Cancellation intentionally may: it retires a not-yet-seen start.
        let mobile = if cancel || self.start_finished.load(Ordering::Acquire) {
            Some(self.session.exchange(if cancel {
                EnrollmentCommand::EnrollmentCancel { operation }
            } else {
                EnrollmentCommand::EnrollmentStatus { operation }
            }))
        } else {
            None
        };
        let mut result = self.response.lock().unwrap_or_else(|e| e.into_inner());
        let persisted = matches!(
            result.receipt.as_ref().map(|r| &r.evidence),
            Some(EnrollmentEvidence::MobilePersisted { .. })
        );
        if let Some(mobile) = mobile {
            match mobile {
                Ok(response) if response.error.is_none() => {
                    if !persisted {
                        *result = response;
                    } else {
                        result.error = None;
                        result.progress = response.progress;
                    }
                }
                response => {
                    result.error = Some(match response {
                        Ok(response) => response.error.unwrap(),
                        Err(error) => error,
                    });
                    result.progress = None;
                }
            }
        }
        match desktop {
            Ok(desktop) => {
                if !matches!(
                    result.receipt.as_ref().map(|r| &r.evidence),
                    Some(EnrollmentEvidence::MobilePersisted { .. })
                ) && desktop.receipt.is_some()
                {
                    result.receipt = desktop.receipt;
                }
                if desktop.error.is_some() {
                    result.error = desktop.error;
                    result.progress = None;
                }
            }
            Err(error) => {
                result.error = Some(error);
                result.progress = None;
            }
        }
        Ok(result.clone())
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = local::exchange(
            &self.path,
            &DesktopRequest::Cancel {
                task: self.session.proposal.task,
                operation: self.operation,
            },
        );
        let _ = self.session.exchange(EnrollmentCommand::EnrollmentCancel {
            operation: self.operation,
        });
    }
}
