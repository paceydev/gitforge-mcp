//! The real binary over stdio, with no forge to talk to: the handshake, the
//! always-on tool, and what it says when the forge can't be reached.

mod common;

use rmcp::model::CallToolRequestParams;

#[tokio::test]
async fn unreachable_forge_offers_only_forge_context() {
    let dir = tempfile::tempdir().unwrap();
    // Port 9 (discard) refuses connections on any normal machine.
    common::git_checkout(dir.path(), "http://127.0.0.1:9/acme/widgets.git");
    let client = common::start(dir.path(), &[("GITFORGE_TOKEN_127_0_0_1_9", "unused")]).await;

    let info = client.peer_info().expect("server info");
    assert_eq!(info.server_info.as_ref().unwrap().name, "gitforge-mcp");
    assert!(
        info.instructions
            .as_deref()
            .unwrap_or_default()
            .contains("forge_context")
    );

    assert_eq!(common::tool_names(&client).await, ["forge_context"]);

    let report = common::forge_context(&client, false).await;
    assert_eq!(report["status"], "unreachable");
    assert_eq!(report["binding"]["repo"]["owner"], "acme");
    assert_eq!(report["binding"]["repo"]["name"], "widgets");
    assert_eq!(
        report["credential"]["source"],
        "env GITFORGE_TOKEN_127_0_0_1_9"
    );
    assert_eq!(report["tools"], serde_json::json!(["forge_context"]));
    let problems = report["problems"].to_string();
    assert!(problems.contains("unreachable"), "{problems}");
    // The token never appears in anything the server says.
    assert!(!report.to_string().contains("unused"));

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn outside_a_repository_reports_unbound() {
    let dir = tempfile::tempdir().unwrap();
    let client = common::start(dir.path(), &[]).await;
    assert_eq!(common::tool_names(&client).await, ["forge_context"]);
    let report = common::forge_context(&client, false).await;
    assert_eq!(report["status"], "unbound");
    assert!(
        report["problems"][0]
            .as_str()
            .unwrap()
            .contains("not inside a git repository")
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn tools_that_are_not_offered_cannot_be_called() {
    let dir = tempfile::tempdir().unwrap();
    let client = common::start(dir.path(), &[]).await;
    let err = client
        .call_tool(CallToolRequestParams::new("ci_status"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not available"), "{err}");
    client.cancel().await.unwrap();
}
