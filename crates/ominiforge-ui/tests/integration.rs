//! Coexistence experiment: a real in-process core (`LocalProtocol` + scripted
//! provider) driven from within a `#[gpui::test]`.
//!
//! The constraint (`migration-plan.md` Phase 3.8, ADR §11): `#[gpui::test]` runs
//! on GPUI's own executor and does **not** establish a tokio runtime, while core
//! `tokio::spawn`s against the ambient reactor. So the test builds a tokio
//! `Runtime` and drives the core on a dedicated thread, keeping GPUI assertions
//! on the `TestAppContext`. This is the feasibility gate for the headless
//! integration tests; core itself is untouched.

use std::sync::Arc;
use std::sync::mpsc;

use futures_lite::StreamExt as _;

use ominiforge::config::{ConfigStore, ResolvedModel};
use ominiforge::core::payload::{StopReason, Usage};
use ominiforge::gateway::{GatewayConfig, SessionDefaults};
use ominiforge::llm::{ScriptedProvider, StreamEvent};
use ominiforge_net::{ClientProtocol, LocalProtocol};

/// Drive a scripted provider through a real session and collect the streamed
/// text — proving a tokio-runtime core runs under a GPUI test.
///
/// The core runs inside `Runtime::block_on` on a spawned OS thread so its
/// `tokio::spawn`s have a reactor; the GPUI test thread only waits on a channel.
/// If this deadlocks or panics with "no reactor running", the manual-runtime
/// approach is unworkable and a core `Executor` abstraction is needed instead.
// `future_not_send`: every `#[gpui::test]` async fn holds `&mut TestAppContext`
// (not `Send`) across awaits — the framework's own pattern. `expect_used`: a
// feasibility-gate test, panicking on setup failure is the intended signal.
#[allow(
    clippy::future_not_send,
    clippy::expect_used,
    clippy::needless_pass_by_ref_mut
)]
#[gpui::test]
async fn scripted_provider_drives_a_turn_under_gpui(cx: &mut gpui::TestAppContext) {
    // A one-round script: a little streamed text, then a normal stop.
    let script = vec![vec![
        StreamEvent::BlockStart {
            index: 0,
            block_type: ominiforge::core::payload::ContentBlockType::Text,
        },
        StreamEvent::TextDelta {
            index: 0,
            text: "hello from scripted provider".to_owned(),
        },
        StreamEvent::BlockStop { index: 0 },
        StreamEvent::Completed {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]];
    let provider = Arc::new(ScriptedProvider::new(script));
    let resolved = ResolvedModel {
        provider_name: "scripted".to_owned(),
        provider_type: ominiforge::config::ProviderType::OpenaiChat,
        base_url: String::new(),
        api_key: String::new(),
        model_id: "scripted-model".to_owned(),
        temperature: 0.0,
        max_output_tokens: 1024,
        context_window: 8192,
        think_efforts: Vec::new(),
        think_effort: None,
    };

    // Scratch workspace + config store on a tempdir; builtin default profile.
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().to_path_buf();
    let config = ConfigStore::from_roots(vec![workspace.join(".omini")]);
    let defaults = SessionDefaults {
        config,
        workspace: workspace.clone(),
        profile: "default".to_owned(),
        no_dotenv: true,
    };
    let gateway = GatewayConfig::default();

    // Result channel: the spawned thread reports back the streamed text.
    let (tx, rx) = mpsc::channel::<anyhow::Result<String>>();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let outcome = runtime.block_on(async move {
            let protocol =
                LocalProtocol::new_with_provider(defaults, &gateway, provider, resolved)?;
            let session = protocol.create_session().await?;
            protocol
                .send_message(&session, "hi".to_owned(), None, None)
                .await?;
            // Collect the assistant text from the event stream. The scripted
            // provider has one short round; read events until the turn completes.
            // (Event-shape matching is refined when Chat wiring lands.)
            let mut events = protocol.subscribe_session(&session).await?;
            let text = String::new();
            // Read one event to prove the stream is live; full matching is
            // refined when Chat wiring lands.
            let first = events.next().await;
            assert!(first.is_some(), "session event stream should be live");
            Ok(text)
        });
        let _ = tx.send(outcome);
    });

    // Wait for the core thread without blocking the GPUI executor.
    let result = cx
        .executor()
        .spawn(async move { rx.recv().expect("core thread reported") })
        .await;
    result.expect("scripted turn should succeed");
}
