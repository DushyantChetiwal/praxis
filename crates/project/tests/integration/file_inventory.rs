use anyhow::Result;
use collections::BTreeSet;
use fs::{FakeFs, Fs as _, RealFs};
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
use util::{path, paths::PathStyle, rel_path::rel_path, test::TempTree};

fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
}

#[gpui::test]
async fn test_native_file_inventory_preserves_host_names_and_ignore_policy(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.executor().allow_parking();
    let directory = TempTree::new(json!({
        ".gitignore": "*.ignored\nignored-dir/\n",
        "ignored-dir": { "old.rs": "ignored subtree" },
        "keep.ignored": "explicit inclusion",
        "hidden.ignored": "ignored"
    }));
    let mut names = vec!["space name.rs", "資料.rs"];
    let mut ignored_names = vec!["hidden.ignored"];
    if !cfg!(windows) {
        names.extend([
            "literal\\name.rs",
            "literal*.rs",
            "literal?.rs",
            "archive:Zone.Identifier",
        ]);
        ignored_names.extend([
            "hidden\\name.ignored",
            "hidden?.ignored",
            "hidden:stamp.ignored",
        ]);
    }
    for name in names.iter().chain(&ignored_names) {
        std::fs::write(directory.path().join(name), "contents").unwrap();
    }
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.file_scan_inclusions =
                    Some(SplicingVec::from(vec!["**/keep.ignored".to_string()]));
            });
        });
    });
    let project = Project::test(RealFs::new(None, cx.executor()), [directory.path()], cx).await;
    let tree = project.read_with(cx, |project, cx| {
        project.visible_worktrees(cx).next().unwrap()
    });
    let declarations: Vec<_> = names
        .iter()
        .chain(&ignored_names)
        .map(|name| name.to_string())
        .collect();
    let inventory = tree
        .update(cx, |tree, cx| tree.file_inventory(declarations.clone(), cx))
        .await
        .unwrap();
    for name in &names {
        assert!(
            inventory.files.iter().any(|file| file.as_str() == *name),
            "missing {name:?}"
        );
    }
    for name in &ignored_names {
        assert!(
            !inventory.files.iter().any(|file| file.as_str() == *name),
            "discovered ignored {name:?}"
        );
        assert!(
            inventory
                .skipped_paths
                .iter()
                .any(|file| file.as_str() == *name),
            "missing scope marker {name:?}"
        );
    }
    for name in &declarations {
        assert!(
            inventory.canonical_paths.contains_key(name),
            "missing declared alias {name:?}"
        );
    }
    assert!(inventory.files.contains(&"keep.ignored".to_string()));
    assert!(inventory.skipped_paths.contains(&"ignored-dir".to_string()));
    std::fs::write(directory.path().join("new file.rs"), "created").unwrap();
    let updated = tree
        .update(cx, |tree, cx| tree.file_inventory(Vec::new(), cx))
        .await
        .unwrap();
    assert!(updated.files.contains(&"new file.rs".to_string()));
    let planned = tree
        .update(cx, |tree, cx| {
            tree.file_inventory(
                vec![
                    "future/file.rs".into(),
                    "missing*.rs".into(),
                    "future:NUL?.txt".into(),
                ],
                cx,
            )
        })
        .await
        .unwrap();
    assert!(!planned.canonical_paths.contains_key("future/file.rs"));
    drop(tree);
    drop(project);
    cx.run_until_parked();
}

#[gpui::test]
async fn test_remote_wire_inventory_preserves_host_paths_with_and_without_wsl_interop(
    cx: &mut TestAppContext,
) {
    struct InventoryClient {
        handlers: Mutex<rpc::ProtoMessageHandlerSet>,
        inventory: rpc::proto::WorktreeFileInventory,
        declarations: Mutex<Vec<String>>,
        wsl_interop: bool,
    }

    impl rpc::ProtoClient for InventoryClient {
        fn request(
            &self,
            envelope: rpc::proto::Envelope,
            _: &'static str,
        ) -> futures::future::BoxFuture<'static, Result<rpc::proto::Envelope>> {
            let Some(rpc::proto::envelope::Payload::ExpandProjectEntry(request)) = envelope.payload
            else {
                return future::ready(Err(anyhow::anyhow!("Unexpected inventory fixture request")))
                    .boxed();
            };
            let request = request.file_inventory.expect("inventory request");
            *self.declarations.lock() = request.declared_paths;
            let response = rpc::proto::ExpandProjectEntryResponse {
                worktree_scan_id: 0,
                supports_file_inventory: true,
                file_inventory: (!request.check_support_only).then(|| self.inventory.clone()),
            };
            future::ready(Ok(rpc::proto::Envelope {
                payload: Some(rpc::proto::envelope::Payload::ExpandProjectEntryResponse(
                    response,
                )),
                ..Default::default()
            }))
            .boxed()
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
            self.wsl_interop
        }
    }

    init_test(cx);
    for (path_style, wsl_interop) in [
        (PathStyle::Windows, false),
        (PathStyle::Unix, false),
        (PathStyle::Unix, true),
    ] {
        let root = if path_style.is_windows() {
            "C:\\remote\\project"
        } else {
            "/remote/project"
        };
        let mut files = vec!["src/space name.rs".to_string(), "資料.rs".to_string()];
        if path_style.is_posix() {
            files.extend(
                [
                    "src/literal\\name.rs",
                    "literal*.rs",
                    "literal?.rs",
                    "archive:Zone.Identifier",
                ]
                .map(String::from),
            );
        }
        let client = Arc::new(InventoryClient {
            handlers: Mutex::default(),
            inventory: rpc::proto::WorktreeFileInventory {
                root_path: root.into(),
                entry_count: files.len() as u64,
                files: files.clone(),
                skipped_paths: Vec::new(),
                canonical_paths: Default::default(),
            },
            declarations: Mutex::default(),
            wsl_interop,
        });
        let tree = cx.update(|cx| {
            Worktree::remote(
                1,
                clock::ReplicaId::new(1),
                rpc::proto::WorktreeMetadata {
                    id: 1,
                    root_name: "project".into(),
                    visible: true,
                    abs_path: root.into(),
                    root_repo_common_dir: None,
                    root_repo_is_linked_worktree: false,
                },
                rpc::AnyProtoClient::new(client.clone()),
                path_style,
                cx,
            )
        });
        tree.update(cx, |tree, cx| tree.negotiate_file_inventory(cx))
            .await;
        tree.read_with(cx, |tree, _| {
            assert_eq!(tree.path_style(), path_style);
        });
        let mut declarations = files.clone();
        declarations.extend(["future/file.rs", "future:NUL?.txt"].map(String::from));
        let inventory = tree
            .update(cx, |tree, cx| tree.file_inventory(declarations.clone(), cx))
            .await
            .unwrap();
        assert_eq!(inventory.files, files);
        assert_eq!(*client.declarations.lock(), declarations);
        assert_eq!(inventory.root_path, PathBuf::from(root));
        drop(tree);
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_qualified_future_path_is_not_redirected_to_an_existing_shadow(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/"),
        json!({
            "first": { "second": { "target.rs": "shadow file" } },
            "second": {}
        }),
    )
    .await;
    let project = Project::test(
        fs,
        [Path::new(path!("/first")), Path::new(path!("/second"))],
        cx,
    )
    .await;
    project.read_with(cx, |project, cx| {
        for input in ["second/target.rs", "SECOND/target.rs"] {
            let resolved = project.find_project_path(input, cx).unwrap();
            let tree = project.worktree_for_id(resolved.worktree_id, cx).unwrap();
            assert_eq!(tree.read(cx).root_name_str(), "second");
            assert_eq!(resolved.path.as_unix_str(), "target.rs");
            assert!(project.entry_for_path(&resolved, cx).is_none());
        }
    });
}

#[gpui::test]
async fn test_project_path_resolution_uses_host_rules_for_missing_files(cx: &mut TestAppContext) {
    init_test(cx);
    for (path_style, root, inputs) in [
        (
            PathStyle::Unix,
            "/remote",
            vec![
                ("remote/new\\name.rs", "new\\name.rs"),
                ("./remote//new\\name.rs", "new\\name.rs"),
                ("/remote/new\\name.rs", "new\\name.rs"),
            ],
        ),
        (
            PathStyle::Windows,
            "C:\\remote",
            vec![
                ("remote\\new\\name.rs", "new/name.rs"),
                ("remote/new\\name.rs", "new/name.rs"),
                ("C:/remote/new\\name.rs", "new/name.rs"),
            ],
        ),
    ] {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [], cx).await;
        let store = project.read_with(cx, |project, _| project.worktree_store());
        cx.update(|cx| {
            let client = rpc::AnyProtoClient::new(rpc::NoopProtoClient::new());
            store.update(cx, |store, _| {
                *store = project::worktree_store::WorktreeStore::remote(
                    true,
                    client.clone(),
                    1,
                    path_style,
                    Default::default(),
                );
            });
            let tree = Worktree::remote(
                1,
                clock::ReplicaId::new(1),
                rpc::proto::WorktreeMetadata {
                    id: 1,
                    root_name: "remote".into(),
                    visible: true,
                    abs_path: root.into(),
                    root_repo_common_dir: None,
                    root_repo_is_linked_worktree: false,
                },
                client,
                path_style,
                cx,
            );
            store.update(cx, |store, cx| store.add(&tree, cx));
            let reads = (fs.read_dir_call_count(), fs.metadata_call_count());
            for (input, expected) in inputs {
                let resolved = project.read(cx).find_project_path(input, cx).unwrap();
                assert_eq!(resolved.path.as_unix_str(), expected);
                assert_eq!(resolved.worktree_id, tree.read(cx).id());
            }
            assert_eq!(reads, (fs.read_dir_call_count(), fs.metadata_call_count()));
        });
    }
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
async fn test_remote_file_inventory_does_not_validate_declared_file_types(
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
    let local_inventory = host_tree
        .update(host_cx, |tree, cx| {
            tree.file_inventory(vec!["src".into()], cx)
        })
        .await
        .unwrap();
    assert!(!local_inventory.canonical_paths.contains_key("src"));
    let remote_inventory = remote_tree
        .update(cx, |tree, cx| tree.file_inventory(vec!["src".into()], cx))
        .await
        .unwrap();
    assert!(!remote_inventory.canonical_paths.contains_key("src"));
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
