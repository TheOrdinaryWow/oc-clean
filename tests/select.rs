#[allow(dead_code)]
#[path = "support/fixture.rs"]
mod fixture;

mod select {
    mod orphans {
        use std::{collections::BTreeSet, fs};

        use oc_clean::analyze::orphans::analyze;
        use oc_clean::assets::storage::{SweepScope, sweep};
        use oc_clean::db::{ConnectionOptions, ReadOnlyConnection, open_read_only};
        use oc_clean::paths::{Target, derived_paths};
        use oc_clean::select::orphans::{
            DanglingSessionId, EventAggregateId, dangling_session_ids, orphan_event_aggregate_ids,
            select,
        };
        use rusqlite::params;

        use super::super::fixture::{Fixture, FixtureConfig};

        fn open_fixture(fixture: &Fixture) -> ReadOnlyConnection {
            open_read_only(
                &Target::File(fixture.database_path.clone()),
                ConnectionOptions::default(),
            )
            .expect("fixture should open read-only")
        }

        #[test]
        fn returns_strict_session_shaped_orphan_event_ids_and_excludes_live_events() {
            let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
            let connection = fixture.connect().expect("fixture should connect");
            for aggregate_id in ["ses_Missing123", "ses_has_underscore", "prj_something"] {
                connection
                    .execute(
                        "INSERT INTO event_sequence VALUES (?1, 1, NULL)",
                        [aggregate_id],
                    )
                    .expect("event sequence should insert");
            }
            drop(connection);

            let ids = orphan_event_aggregate_ids(&open_fixture(&fixture))
                .expect("orphan event selection should succeed");

            assert_eq!(
                ids.iter().map(EventAggregateId::as_str).collect::<Vec<_>>(),
                vec!["ses_Missing123"]
            );
        }

        #[test]
        fn sweeps_a_dangling_parent_chain_to_fixed_point_in_one_call() {
            let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
            let connection = fixture.connect().expect("fixture should connect");
            for (id, parent_id) in [
                ("ses_DanglingA", "ses_Gone"),
                ("ses_DanglingB", "ses_DanglingA"),
                ("ses_DanglingC", "ses_DanglingB"),
            ] {
                connection
                    .execute(
                        "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated) VALUES (?1, 'project-0', ?2, ?1, '/tmp', ?1, '1', 1, 1)",
                        params![id, parent_id],
                    )
                    .expect("dangling session should insert");
            }
            drop(connection);

            let ids = dangling_session_ids(&open_fixture(&fixture))
                .expect("dangling session selection should succeed");

            assert_eq!(
                ids.iter()
                    .map(DanglingSessionId::as_str)
                    .collect::<Vec<_>>(),
                vec!["ses_DanglingA", "ses_DanglingB", "ses_DanglingC"]
            );
        }

        #[test]
        fn returns_orphan_storage_files_and_snapshot_directories() {
            let fixture = Fixture::build(&FixtureConfig {
                orphan_storage_file_count: 2,
                orphan_snapshot_dir_count: 2,
                ..FixtureConfig::default()
            })
            .expect("fixture should build");

            let selected = select(&open_fixture(&fixture), &derived_paths(fixture.root()))
                .expect("orphan selection should succeed");
            let expected_snapshots = fixture
                .orphan_snapshot_dirs
                .iter()
                .map(|path| {
                    path.parent()
                        .expect("snapshot has project directory")
                        .to_path_buf()
                })
                .collect::<BTreeSet<_>>();

            assert_eq!(
                selected.storage_files,
                fixture.orphan_storage_files.iter().cloned().collect()
            );
            assert_eq!(selected.snapshot_directories, expected_snapshots);
        }

        #[test]
        fn strict_storage_names_match_between_preview_analysis_and_apply() {
            let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
            let paths = derived_paths(fixture.root());
            let bucket = paths.storage.join("regression");
            fs::create_dir_all(&bucket).expect("storage bucket should be created");
            let valid = bucket.join("ses_abc123.json");
            let invalid = bucket.join("ses_bad id.json");
            fs::write(&valid, b"valid orphan").expect("valid orphan should be written");
            fs::write(&invalid, b"invalid orphan").expect("invalid orphan should be written");

            let database = open_fixture(&fixture);
            let selected = select(&database, &paths).expect("orphan selection should succeed");
            let census = analyze(&database, &paths).expect("orphan census should succeed");

            assert_eq!(selected.storage_files, BTreeSet::from([valid.clone()]));
            assert_eq!(census.orphan_storage_files.count, 1);
            assert!(!selected.storage_files.contains(&invalid));

            let report = sweep(&database, &paths.storage, SweepScope::AllOrphans)
                .expect("orphan storage sweep should succeed");
            let applied = [valid.clone(), invalid.clone()]
                .into_iter()
                .filter(|path| !path.exists())
                .collect::<BTreeSet<_>>();

            assert_eq!(report.deleted_files, census.orphan_storage_files.count);
            assert_eq!(applied, selected.storage_files);
            assert_eq!(applied, BTreeSet::from([valid]));
            assert!(invalid.exists());
        }

        #[test]
        fn raw_orphans_are_additive_to_an_independent_age_candidate_set() {
            let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
            let connection = fixture.connect().expect("fixture should connect");
            connection
                .execute(
                    "INSERT INTO event_sequence VALUES ('ses_MissingAdditive', 1, NULL)",
                    [],
                )
                .expect("orphan event should insert");
            drop(connection);
            let age_candidates = BTreeSet::from([fixture.session_ids[0].clone()]);

            let selected = select(&open_fixture(&fixture), &derived_paths(fixture.root()))
                .expect("orphan selection should succeed");

            assert_eq!(age_candidates.len(), 1);
            assert_eq!(
                selected
                    .event_aggregate_ids
                    .iter()
                    .map(EventAggregateId::as_str)
                    .collect::<Vec<_>>(),
                vec!["ses_MissingAdditive"]
            );
        }
    }
}
