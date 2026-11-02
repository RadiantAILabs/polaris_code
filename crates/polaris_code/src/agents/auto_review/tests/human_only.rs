//! Calls only a person may approve stay with a person, whatever the reviewer
//! would answer. Every case scripts a reviewer that says `allow`, so a call that
//! still reaches a person, and never reaches the model, shows the reviewer was
//! kept out of the decision.

use super::*;
use crate::tools::{
    AttendedApproval, ToolPolicy, ToolPolicyPlugin, WEB_FETCH, WebToolsConfig, WebToolsPlugin,
};
use polaris::models::llm::ToolDefinition;
use polaris::system::plugin::{Contract, PluginAccess, VersionReq};
use polaris::tools::{Tool, ToolContext, ToolError};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

const ALLOW: &str = r#"{"decision":"allow","reason":"Matches the request"}"#;

/// The reviewer is never asked, the person is asked once and refuses, and the
/// trajectory names the person as the one who decided.
fn assert_left_to_a_person(h: &Harness, tool: &str) {
    assert!(
        h.requests.lock().unwrap().is_empty(),
        "the reviewer was consulted"
    );
    assert_eq!(h.human.prompts(), 1, "{}", h.human.text());
    assert!(
        h.human
            .text()
            .contains("This call needs a person's approval"),
        "{}",
        h.human.text()
    );
    assert!(
        !h.human.text().contains("auto-approved"),
        "{}",
        h.human.text()
    );
    assert!(h.review().is_none(), "no review is recorded for the call");
    let history =
        serde_json::to_string(&h.ctx.get_resource::<ContextManager>().unwrap().messages).unwrap();
    assert!(
        history.contains(&format!("User denied execution of tool '{tool}'")),
        "{history}"
    );
    let refs = h.policy_refs();
    assert!(
        refs.iter()
            .any(|r| r == "authorization.decision_source=human"),
        "{refs:?}"
    );
    assert!(
        refs.iter()
            .all(|r| !r.starts_with("authorization.reviewer=")),
        "{refs:?}"
    );
}

#[tokio::test]
async fn an_explicit_ask_rule_is_answered_by_a_person() {
    let mut h = Harness::with(
        ALLOW,
        false,
        false,
        None,
        Some("[[rules]]\ntool = 'run_command'\ndecision = 'ask'\ncommand_prefix = 'touch'\n"),
        |_| {},
    )
    .await;
    h.set_call("run_command", json!({"command":"touch marker"}));
    h.run(true).await;
    assert_left_to_a_person(&h, "run_command");
    assert!(!h.root.path().join("marker").exists());
}

#[tokio::test]
async fn an_untrusted_project_allow_rule_is_answered_by_a_person_until_it_is_trusted() {
    let mut h = Harness::with(
        ALLOW,
        false,
        false,
        None,
        Some("[[rules]]\ntool = 'run_command'\ndecision = 'allow'\ncommand_prefix = 'touch'\n"),
        |_| {},
    )
    .await;
    h.set_call("run_command", json!({"command":"touch marker"}));
    h.run(true).await;
    assert_left_to_a_person(&h, "run_command");
    assert!(!h.root.path().join("marker").exists());

    // Once the person trusts the project's rules, the same rule allows the call
    // outright: neither a prompt nor the reviewer is involved.
    h.ctx
        .get_resource::<PermissionRules>()
        .unwrap()
        .trust_project();
    h.run(true).await;
    assert!(h.root.path().join("marker").exists());
    assert_eq!(h.human.prompts(), 1, "only the first call asked");
    assert!(h.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn web_fetch_is_answered_by_a_person_even_when_a_trusted_rule_allows_it() {
    for rules in [
        None,
        Some("[[rules]]\ntool = 'web_fetch'\ndecision = 'allow'\n"),
    ] {
        let mut h = Harness::with(ALLOW, false, false, None, rules, |server| {
            server.add_plugins(WebToolsPlugin::new(WebToolsConfig::new()));
        })
        .await;
        if rules.is_some() {
            h.ctx
                .get_resource::<PermissionRules>()
                .unwrap()
                .trust_project();
        }
        // A loopback address, so even a wrongly granted call reaches no network.
        h.set_call(WEB_FETCH, json!({"url":"http://127.0.0.1:9/page"}));
        h.run(true).await;
        assert_left_to_a_person(&h, WEB_FETCH);
    }
}

/// A tool that counts its runs and declares, through the public policy
/// registry, that an attended call needs a person.
struct Remote(Arc<AtomicUsize>);

impl Tool for Remote {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "remote_lookup",
            "Looks something up on a remote service.",
            json!({"type":"object","properties":{}}),
        )
    }
    fn permission(&self) -> ToolPermission {
        ToolPermission::Confirm
    }
    fn execute<'ctx>(
        &'ctx self,
        _args: Value,
        _ctx: &'ctx ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'ctx>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!("done"))
        })
    }
}

struct RemotePlugin(Arc<AtomicUsize>);

impl Plugin for RemotePlugin {
    const ID: &'static str = "test::remote_tool";
    const VERSION: Version = Version::new(0, 1, 0);
    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<ToolsPlugin>(),
            PluginId::of::<ToolPolicyPlugin>(),
        ]
    }
    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
            .extends::<ToolPolicyRegistry>(VersionReq::caret(ToolPolicyRegistry::CONTRACT_VERSION))
    }
    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ToolRegistry>()
            .unwrap()
            .register(Remote(self.0.clone()));
        server
            .get_resource_mut::<ToolPolicyRegistry>()
            .unwrap()
            .declare(
                "remote_lookup",
                ToolPolicy::default().with_attended(AttendedApproval::Human),
            );
    }
}

#[tokio::test]
async fn any_capability_can_require_a_person_through_the_public_policy_registry() {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut h = Harness::with(ALLOW, false, false, None, None, {
        let runs = runs.clone();
        move |server| {
            server.add_plugins(RemotePlugin(runs));
        }
    })
    .await;
    h.set_call("remote_lookup", json!({}));
    h.run(true).await;
    assert_left_to_a_person(&h, "remote_lookup");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}
