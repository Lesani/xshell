//! Permission Prompt fixtures (`tests/fixtures/prompts`, see its README) fed through the
//! screen model at their size and read by the extractor.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use xshell_core::agent_status::HookAgent;
use xshell_core::prompt::{composer, extract, screen_tail, Found, ScreenModel};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prompts")
}

struct Fixture {
    name: String,
    agent: HookAgent,
    cols: u16,
    rows: u16,
    meta: Value,
    raw: Vec<u8>,
}

impl Fixture {
    fn load(name: &str) -> Fixture {
        let meta: Value =
            serde_json::from_slice(&fs::read(dir().join(format!("{name}.json"))).unwrap()).unwrap();
        let agent = match meta["agent"].as_str().unwrap() {
            "claude" => HookAgent::Claude,
            "codex" => HookAgent::Codex,
            a => panic!("{name}: agent {a}"),
        };
        Fixture {
            name: name.into(),
            agent,
            cols: meta["cols"].as_u64().unwrap() as u16,
            rows: meta["rows"].as_u64().unwrap() as u16,
            raw: fs::read(dir().join(format!("{name}.raw"))).unwrap(),
            meta,
        }
    }

    fn all() -> Vec<Fixture> {
        let mut names: Vec<String> = fs::read_dir(dir())
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                (p.extension()? == "raw").then(|| p.file_stem().unwrap().to_string_lossy().into())
            })
            .collect();
        names.sort();
        assert!(names.len() >= 9, "{names:?}");
        names.iter().map(|n| Fixture::load(n)).collect()
    }

    fn screen(&self) -> Vec<String> {
        let mut m = ScreenModel::new(self.cols, self.rows);
        m.feed(&self.raw);
        m.rows()
    }

    /// Fed in small pieces, as a PTY read may split it anywhere (inside escape sequences and
    /// UTF-8 characters too).
    fn screen_in_pieces(&self, n: usize) -> Vec<String> {
        let mut m = ScreenModel::new(self.cols, self.rows);
        for piece in self.raw.chunks(n) {
            m.feed(piece);
        }
        m.rows()
    }

    fn found(&self) -> Option<Found> {
        extract(self.agent, &self.screen())
    }

    fn expect(&self) -> Option<(Vec<String>, Vec<String>)> {
        let e = &self.meta["expect"];
        if e.is_null() {
            return None;
        }
        let strs = |k: &str| -> Vec<String> {
            e[k].as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_string())
                .collect()
        };
        Some((strs("textContains"), strs("options")))
    }
}

fn labels(f: &Found) -> Vec<String> {
    f.options.iter().map(|o| o.label.clone()).collect()
}

fn check_prompts(agent: HookAgent) {
    let mut checked = 0;
    for fx in Fixture::all().into_iter().filter(|f| f.agent == agent) {
        let Some((text, options)) = fx.expect() else {
            continue;
        };
        let f = fx
            .found()
            .unwrap_or_else(|| panic!("{}: no prompt on\n{}", fx.name, fx.screen().join("\n")));
        assert_eq!(labels(&f), options, "{}", fx.name);
        let keys: Vec<Vec<u8>> = f.options.iter().map(|o| o.keys.clone()).collect();
        let want: Vec<Vec<u8>> = (1..=options.len()).map(|i| vec![b'0' + i as u8]).collect();
        assert_eq!(keys, want, "{}", fx.name);
        for t in &text {
            assert!(
                f.text.contains(t.as_str()),
                "{}: {t:?} not in {:?}",
                fx.name,
                f.text
            );
        }
        // However the output was split.
        for n in [1, 7, 64] {
            assert_eq!(
                extract(fx.agent, &fx.screen_in_pieces(n)),
                Some(f.clone()),
                "{}",
                fx.name
            );
        }
        checked += 1;
    }
    assert!(checked >= 2, "{agent:?}: {checked} fixtures");
}

#[test]
fn fixtures_are_marked_synthetic_or_recorded() {
    for fx in Fixture::all() {
        let m = &fx.meta;
        assert!(m["version"].is_string(), "{}", fx.name);
        assert!(
            m["synthetic"].is_boolean(),
            "{}: synthetic or recorded?",
            fx.name
        );
    }
}

#[test]
fn recorded_claude_prompts_extract() {
    check_prompts(HookAgent::Claude);
}

#[test]
fn recorded_codex_prompts_extract() {
    check_prompts(HookAgent::Codex);
}

#[test]
fn narrow_screen_joins_wrapped_labels() {
    for (narrow, wide) in [
        ("claude-bash-50x30", "claude-bash-100x30"),
        ("codex-exec-50x30", "codex-exec-100x30"),
    ] {
        let (n, w) = (Fixture::load(narrow), Fixture::load(wide));
        // The narrow screen really wraps a label.
        let wrapped = n.screen().iter().any(|r| {
            let t = r.trim_start();
            !t.is_empty()
                && r.len() - t.len() >= 5
                && !t.starts_with(|c: char| c.is_ascii_digit())
                && (t.contains("differently") || t.contains("proj") || t.contains("session"))
        });
        assert!(
            wrapped,
            "{narrow} wraps no label:\n{}",
            n.screen().join("\n")
        );
        assert_eq!(labels(&n.found().unwrap()), labels(&w.found().unwrap()));
    }
}

#[test]
fn unknown_screens_are_not_prompts() {
    let mut unknown = 0;
    for fx in Fixture::all().into_iter().filter(|f| f.expect().is_none()) {
        for agent in [HookAgent::Claude, HookAgent::Codex] {
            assert_eq!(extract(agent, &fx.screen()), None, "{}", fx.name);
        }
        unknown += 1;
    }
    assert!(unknown >= 2);
    // Every prompt fixture is its own agent's only.
    for fx in Fixture::all().into_iter().filter(|f| f.expect().is_some()) {
        let other = match fx.agent {
            HookAgent::Claude => HookAgent::Codex,
            HookAgent::Codex => HookAgent::Claude,
        };
        assert_eq!(extract(other, &fx.screen()), None, "{}", fx.name);
    }
}

#[test]
fn elicitation_gives_a_text_only_tail() {
    let fx = Fixture::load("unknown-elicitation");
    let t = screen_tail(&fx.screen());
    assert!(t.contains("Which project should the issue go to?"), "{t}");
    assert!(t.ends_with("Enter to submit · Esc to decline"), "{t}");
    assert!(!t.contains('│') && !t.contains('─'), "{t}");
}

#[test]
fn screen_model_follows_resize() {
    // The 50-column dialog drawn after the screen shrank from 100 columns reads like the
    // 50-column fixture.
    let (n, w) = (
        Fixture::load("claude-bash-50x30"),
        Fixture::load("claude-bash-100x30"),
    );
    let mut m = ScreenModel::new(w.cols, w.rows);
    m.feed(&w.raw);
    m.resize(n.cols, n.rows);
    assert_eq!(m.size(), (n.cols, n.rows));
    m.feed(b"\x1b[2J\x1b[H");
    m.feed(&n.raw);
    assert_eq!(extract(n.agent, &m.rows()), n.found());
}

fn other(agent: HookAgent) -> HookAgent {
    match agent {
        HookAgent::Claude => HookAgent::Codex,
        HookAgent::Codex => HookAgent::Claude,
    }
}

/// Each fixture says whether its screen ends with the agent's chat composer: idle, working
/// and with a draft it does; a Permission Prompt, a model picker, the bash mode and a form do
/// not. Another agent's composer is never this one's.
#[test]
fn composer_is_recognised_only_where_marked() {
    let mut yes = std::collections::HashMap::new();
    for fx in Fixture::all() {
        let want = fx.meta["composer"]
            .as_bool()
            .unwrap_or_else(|| panic!("{}: composer?", fx.name));
        let screen = fx.screen();
        assert_eq!(
            composer(fx.agent, &screen),
            want,
            "{}:\n{}",
            fx.name,
            screen.join("\n")
        );
        assert!(!composer(other(fx.agent), &screen), "{}", fx.name);
        for n in [1, 7, 64] {
            assert_eq!(
                composer(fx.agent, &fx.screen_in_pieces(n)),
                want,
                "{}",
                fx.name
            );
        }
        // A composer and a Permission Prompt never show together.
        if want {
            assert_eq!(fx.found(), None, "{}", fx.name);
            *yes.entry(format!("{:?}", fx.agent)).or_insert(0) += 1;
        }
    }
    assert!(yes.values().all(|n| *n >= 3) && yes.len() == 2, "{yes:?}");
}

/// Every composer fixture turns bracketed paste on as the agents do; the screen model
/// tracks it.
#[test]
fn bracketed_paste_follows_the_output() {
    let fx = Fixture::load("claude-composer-idle");
    let mut m = ScreenModel::new(fx.cols, fx.rows);
    m.feed(&fx.raw);
    assert!(!m.bracketed_paste());
    m.feed(b"\x1b[?2004h");
    assert!(m.bracketed_paste());
    m.feed(&fx.raw);
    assert!(m.bracketed_paste(), "drawing keeps it");
    assert!(composer(fx.agent, &m.rows()));
}
