//! Operational access without exposing the elected owner's private Relay handle.
use crate::handle::{InputTarget, UpdateTarget};
use crate::ring::broker::BrokerHostProxy;
use crate::{HostConfig, HostError, HostHandle, HostSnapshot, HostStatus, TermSink, Waiter};
use serde_json::{Map, Value};
use std::sync::Arc;
use uuid::Uuid;
use xshell_protocol::msg::{OpenReply, OpenSpec, TerminalInfo};

#[derive(Clone)]
pub enum HostAccess {
    Direct(Arc<HostHandle>),
    Broker(BrokerHostProxy),
}
macro_rules! delegate {
    ($self:ident, $method:ident $(, $arg:expr)*) => { match $self { Self::Direct(h) => h.$method($($arg),*), Self::Broker(h) => h.$method($($arg),*) } };
}
impl HostAccess {
    pub fn id(&self) -> &str {
        delegate!(self, id)
    }
    pub fn status(&self) -> HostStatus {
        self.snapshot().status
    }
    pub fn snapshot(&self) -> HostSnapshot {
        match self {
            Self::Direct(h) => h.access_snapshot(),
            Self::Broker(h) => h.snapshot(),
        }
    }
    pub fn terminals(&self) -> Option<Vec<TerminalInfo>> {
        delegate!(self, terminals)
    }
    pub fn link_generation(&self) -> u64 {
        self.status().link_generation
    }
    pub fn saved_config(&self) -> Option<HostConfig> {
        match self {
            Self::Direct(h) => h.saved_config(),
            Self::Broker(_) => None,
        }
    }
    pub fn companion_pending(
        &self,
        request: xshell_protocol::ring::companion::CompanionAdminRequest,
        w: Waiter,
    ) {
        delegate!(self, companion_pending, request, w)
    }
    pub fn call(&self, method: String, params: Value, w: Waiter) {
        delegate!(self, call, method, params, w)
    }
    pub fn term_open(
        &self,
        spec: OpenSpec,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<OpenReply, HostError>) + Send>,
    ) {
        match self {
            Self::Direct(h) => h.access_term_open(h.direct_viewer(), spec, sink, w),
            Self::Broker(h) => h.term_open(spec, sink, w),
        }
    }
    pub fn term_attach(
        &self,
        terminal: Uuid,
        sink: Arc<dyn TermSink>,
        w: Box<dyn FnOnce(Result<Option<i32>, HostError>) + Send>,
    ) {
        match self {
            Self::Direct(h) => h.access_term_attach(h.direct_viewer(), terminal, sink, w),
            Self::Broker(h) => h.term_attach(terminal, sink, w),
        }
    }
    pub fn term_detach(&self, terminal: Uuid) -> Result<(), HostError> {
        match self {
            Self::Direct(h) => h.access_term_detach(h.direct_viewer(), terminal),
            Self::Broker(h) => h.term_detach(terminal),
        }
    }
    pub fn term_input(
        &self,
        terminal: Uuid,
        data: String,
        observed: Option<Uuid>,
        target: Option<InputTarget>,
    ) -> Result<(), HostError> {
        match self {
            Self::Direct(h) => h
                .access_term_input(h.direct_viewer(), terminal, data, observed, target)
                .map(|_| ()),
            Self::Broker(h) => h.term_input(terminal, data, observed, target),
        }
    }
    pub fn term_resize(&self, terminal: Uuid, cols: u16, rows: u16) -> Result<(), HostError> {
        match self {
            Self::Direct(h) => h
                .access_term_resize(h.direct_viewer(), terminal, cols, rows)
                .map(|_| ()),
            Self::Broker(h) => h.term_resize(terminal, cols, rows),
        }
    }
    pub fn term_close(&self, terminal: Uuid, w: Waiter) {
        delegate!(self, term_close, terminal, w)
    }
    pub fn term_update(
        &self,
        terminal: Uuid,
        session_id: Option<String>,
        meta: Option<Map<String, Value>>,
        w: Waiter,
    ) {
        delegate!(self, term_update, terminal, session_id, meta, w)
    }
    pub fn term_relaunch(
        &self,
        terminal: Uuid,
        skip: bool,
        w: Box<dyn FnOnce(Result<Option<u32>, HostError>) + Send>,
    ) {
        delegate!(self, term_relaunch, terminal, skip, w)
    }
    pub fn upgrade(&self, w: Waiter) {
        delegate!(self, upgrade, w)
    }
    pub fn upgrade_target(&self, target: Option<UpdateTarget>, w: Waiter) {
        match self {
            Self::Direct(h) => h.access_upgrade_target(target, w),
            Self::Broker(h) => h.upgrade_target(target, w),
        }
    }
    pub fn kick(&self) {
        delegate!(self, kick)
    }
}
