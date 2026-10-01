use anyhow::Result;
use collections::BTreeSet;
use fs::{FakeFs, Fs as _};
use futures::{FutureExt as _, future};
use gpui::{TestAppContext, UpdateGlobal as _};
use parking_lot::Mutex;
use project::{Project, Worktree};
use serde_json::json;
use settings::{SettingsStore, SplicingVec};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic},
    time::Duration,
};
use util::{path, paths::PathStyle, rel_path::rel_path};

fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
}

#[gpui::test]
async fn test_remote_file_inventory_support_probe_times_out(cx: &mut TestAppContext) {
    struct UnresponsiveClient {
        handlers: Mutex<rpc::ProtoMessageHandlerSet>,
        requests: Arc<atomic::AtomicUsize>,
    }

    impl rpc::ProtoClient for UnresponsiveClient {
        fn request(
            &self,
            _: rpc::proto::Envelope,
            _: &'static str,
        ) -> futures::future::BoxFuture<'static, Result<rpc::proto::Envelope>> {
            self.requests.fetch_add(1, atomic::Ordering::SeqCst);
            future::pending().boxed()
        }

        fn send(&self, _: rpc::proto::Envelope, _: &'static str) -> Result<()> {
            Ok(())
        }

        fn send_response(&self, _: rpc::proto::Envelope, _: &'static str) -> Result<()> {
            Ok(())
        }

        fn message_handler_set(&self) -> &Mutex<rpc::ProtoMessageHandlerSet> {
            &self.handlers
        }

        fn is_via_collab(&self) -> bool {
            false
        }

        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    init_test(cx);
    let requests = Arc::new(atomic::AtomicUsize::new(0));
    let tree = cx.update(|cx| {
        Worktree::remote(
            1,
            clock::ReplicaId::new(1),
            rpc::proto::WorktreeMetadata {
                id: 1,
                root_name: "project".into(),
                visible: true,
                abs_path: "/remote/project".into(),
                root_repo_common_dir: None,
                root_repo_is_linked_worktree: false,
            },
            rpc::AnyProtoClient::new(Arc::new(UnresponsiveClient {
                handlers: Mutex::default(),
                requests: requests.clone(),
            })),
            PathStyle::Unix,
            cx,
        )
    });
    let negotiation = tree.update(cx, |tree, cx| tree.negotiate_file_inventory(cx));
    let concurrent = tree.update(cx, |tree, cx| tree.negotiate_file_inventory(cx));
    cx.run_until_parked();
    assert_eq!(
        requests.load(atomic::Ordering::SeqCst),
        1,
        "simultaneous probes must share one request"
    );
    tree.read_with(cx, |tree, _| {
        assert!(
            tree.check_file_inventory_support()
                .unwrap_err()
                .to_string()
                .contains("still loading")
        );
    });
    cx.executor().advance_clock(Duration::from_secs(31));
    negotiation.await;
    concurrent.await;
    tree.read_with(cx, |tree, _| {
        let error = tree.check_file_inventory_support().unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("Reconnect"), "{error}");
    });
    let retry = tree.update(cx, |tree, cx| tree.negotiate_file_inventory(cx));
    cx.run_until_parked();
    assert_eq!(
        requests.load(atomic::Ordering::SeqCst),
        2,
        "a completed failed probe must allow retry"
    );
    cx.executor().advance_clock(Duration::from_secs(31));
    retry.await;
}

#[gpui::test(iterations = 3)]
async fn test_remote_file_inventory_uses_host_files_and_refresh_barriers(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    init_test(cx);
    let client_fs = FakeFs::new(cx.executor());
    let host_fs = FakeFs::new(host_cx.executor());
    client_fs
        .insert_tree(path!("/a"), json!({ "client_only.rs": "wrong filesystem" }))
        .await;
    host_fs
        .insert_tree(
            path!("/a"),
            json!({
                ".gitignore": "ignored/\n",
                "host_only.rs": "host",
                "ignored": { "old.rs": "not a creation" },
                "target": { "generated.rs": "excluded" }
            }),
        )
        .await;
    init_test(host_cx);
    host_cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.file_scan_exclusions =
                    Some(SplicingVec::from(vec!["**/target".to_string()]));
            });
        });
    });
    let (project, _host) = Project::test_remote_worktrees(
        client_fs.clone(),
        host_fs.clone(),
        [Path::new(path!("/a"))],
        cx,
        host_cx,
    )
    .await;
    let tree = project.read_with(cx, |project, cx| {
        assert!(project.is_via_remote_server());
        project.visible_worktrees(cx).next().unwrap()
    });
    tree.read_with(cx, |tree, _| {
        assert!(tree.is_remote());
        tree.check_file_inventory_support().unwrap();
    });
    let client_reads = (
        client_fs.read_dir_call_count(),
        client_fs.metadata_call_count(),
    );
    let baseline = tree
        .update(cx, |tree, cx| tree.file_inventory(vec![], cx))
        .await
        .unwrap();
    assert_eq!(
        baseline.files.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([".gitignore".to_string(), "host_only.rs".to_string()])
    );
    assert!(baseline.skipped_paths.contains(&"ignored".to_string()));
    assert!(baseline.skipped_paths.contains(&"target".to_string()));

    host_fs.pause_events();
    host_fs
        .insert_tree(
            path!("/a"),
            json!({ "new": { "created.rs": "external write" } }),
        )
        .await;
    host_fs
        .create_symlink(
            Path::new(path!("/a/alias.rs")),
            PathBuf::from(path!("/a/host_only.rs")),
        )
        .await
        .unwrap();
    let inventory = tree
        .update(cx, |tree, cx| {
            tree.file_inventory(vec!["alias.rs".into(), "host_only.rs".into()], cx)
        })
        .await
        .unwrap();
    assert!(inventory.files.contains(&"new/created.rs".to_string()));
    assert_eq!(
        inventory.canonical_paths["alias.rs"],
        inventory.canonical_paths["host_only.rs"]
    );
    tree.read_with(cx, |tree, _| {
        assert!(
            tree.entry_for_path(rel_path("new/created.rs")).is_some(),
            "the inventory response must wait for the replicated scan"
        );
    });
    assert_eq!(
        client_reads,
        (
            client_fs.read_dir_call_count(),
            client_fs.metadata_call_count()
        )
    );
    host_fs.unpause_events_and_flush();
}

#[gpui::test]
async fn test_remote_file_inventory_rejects_directory_declarations(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    init_test(cx);
    let client_fs = FakeFs::new(cx.executor());
    let host_fs = FakeFs::new(host_cx.executor());
    host_fs
        .insert_tree(path!("/a"), json!({ "src": { "main.rs": "host" } }))
        .await;
    let (project, host) =
        Project::test_remote_worktrees(client_fs, host_fs, [Path::new(path!("/a"))], cx, host_cx)
            .await;
    let host_tree = host.read_with(host_cx, |project, cx| {
        project.visible_worktrees(cx).next().unwrap()
    });
    let remote_tree = project.read_with(cx, |project, cx| {
        project.visible_worktrees(cx).next().unwrap()
    });
    let local_error = host_tree
        .update(host_cx, |tree, cx| {
            tree.file_inventory(vec!["src".into()], cx)
        })
        .await
        .unwrap_err();
    assert!(format!("{local_error:#}").contains("is a directory"));
    let remote_error = remote_tree
        .update(cx, |tree, cx| tree.file_inventory(vec!["src".into()], cx))
        .await
        .unwrap_err();
    assert!(format!("{remote_error:#}").contains("is a directory"));
    let inventory = remote_tree
        .update(cx, |tree, cx| {
            tree.file_inventory(vec!["src/main.rs".into()], cx)
        })
        .await
        .unwrap();
    assert!(inventory.files.contains(&"src/main.rs".to_string()));
}

#[gpui::test(iterations = 3)]
async fn test_remote_file_inventory_disconnect_is_not_an_empty_snapshot(
    cx: &mut TestAppContext,
    host_cx: &mut TestAppContext,
) {
    init_test(cx);
    let client_fs = FakeFs::new(cx.executor());
    let host_fs = FakeFs::new(host_cx.executor());
    host_fs
        .insert_tree(path!("/a"), json!({ "host.rs": "host" }))
        .await;
    let (project, _host) = Project::test_remote_worktrees(
        client_fs.clone(),
        host_fs,
        [Path::new(path!("/a"))],
        cx,
        host_cx,
    )
    .await;
    let tree = project.read_with(cx, |project, cx| {
        project.visible_worktrees(cx).next().unwrap()
    });
    let client_reads = (
        client_fs.read_dir_call_count(),
        client_fs.metadata_call_count(),
    );
    let inventory = tree.update(cx, |tree, cx| tree.file_inventory(Vec::new(), cx));
    let remote = project.read_with(cx, |project, _| project.remote_client().unwrap());
    remote.update(cx, |remote, cx| remote.force_server_not_running(cx));
    cx.run_until_parked();
    let error = inventory.await.unwrap_err();
    assert!(format!("{error:#}").contains("disconnect"), "{error:#}");
    tree.read_with(cx, |tree, _| {
        assert!(
            tree.check_file_inventory_support()
                .unwrap_err()
                .to_string()
                .contains("Reconnect")
        );
    });
    assert_eq!(
        client_reads,
        (
            client_fs.read_dir_call_count(),
            client_fs.metadata_call_count()
        )
    );
}
