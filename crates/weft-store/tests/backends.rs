//! One suite, every backend: MemoryStore is the reference semantics and
//! PgStore must be indistinguishable through the traits. The Postgres run
//! gates on `WEFT_TEST_DATABASE_URL` (e.g.
//! `postgres://postgres:weft@127.0.0.1:15432/postgres`) and skips silently
//! when absent so `cargo test` needs no database.

use weft_proto::{Account, MsgId, MsgMeta, RetentionPolicy, Ulid, UserRef};
use weft_store::{
    materialize, AccountStore, CapabilityStore, ChannelStore, EventKind, EventRecord, EventStore,
    HistoryItem, InviteRecord, InviteStore, MemoryStore, NamespaceRecord, NamespaceStore, Page,
    RedeemOutcome, Scope,
};

fn user(name: &str) -> UserRef {
    format!("{name}@test.example").parse().unwrap()
}

fn msgid(at_ms: u64) -> MsgId {
    format!("test.example/{}", Ulid::from_parts(at_ms, at_ms as u128))
        .parse()
        .unwrap()
}

fn record(scope: &Scope, at_ms: u64, root_ms: u64, sender: &str, kind: EventKind) -> EventRecord {
    EventRecord {
        scope: scope.clone(),
        msgid: msgid(at_ms),
        root: msgid(root_ms),
        sender: user(sender),
        kind,
    }
}

fn message(scope: &Scope, at_ms: u64, body: &str) -> EventRecord {
    record(
        scope,
        at_ms,
        at_ms,
        "ada",
        EventKind::Message {
            body: body.to_string(),
            meta: MsgMeta::default(),
        },
    )
}

fn page(limit: usize) -> Page {
    Page {
        before: None,
        after: None,
        limit,
    }
}

/// The whole contract, in one pass. `tag` isolates scopes/accounts so the
/// suite is re-runnable against a persistent database.
async fn suite<S>(store: &S, tag: &str)
where
    S: EventStore + AccountStore + ChannelStore + CapabilityStore + InviteStore + NamespaceStore,
{
    let chan: Scope = Scope::Channel(format!("#suite-{tag}").parse().unwrap());
    let ada: Account = format!("ada-{tag}").parse().unwrap();
    let bob: Account = format!("bob-{tag}").parse().unwrap();

    // -- events: append, page, mutate, materialize --
    for at in [1_000, 2_000, 3_000, 4_000] {
        store
            .append(message(&chan, at, &format!("m{at}")))
            .await
            .unwrap();
    }
    store
        .append(record(
            &chan,
            5_000,
            2_000,
            "ada",
            EventKind::Edit {
                body: "m2000 final".into(),
            },
        ))
        .await
        .unwrap();
    store
        .append(record(
            &chan,
            6_000,
            3_000,
            "bob",
            EventKind::React {
                emoji: "👍".into(),
                add: true,
            },
        ))
        .await
        .unwrap();
    store
        .append(record(&chan, 7_000, 4_000, "ada", EventKind::Delete))
        .await
        .unwrap();

    // Newest-anchored paging, ascending output.
    let last_two = store.roots(&chan, page(2)).await.unwrap();
    assert_eq!(
        last_two.iter().map(|r| r.at_ms()).collect::<Vec<_>>(),
        [3_000, 4_000]
    );
    // Cursor paging backwards.
    let older = store
        .roots(
            &chan,
            Page {
                before: Some(last_two[0].msgid.ulid()),
                after: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        older.iter().map(|r| r.at_ms()).collect::<Vec<_>>(),
        [1_000, 2_000]
    );

    // find_root: roots only.
    assert!(store
        .find_root(msgid(2_000).ulid())
        .await
        .unwrap()
        .is_some());
    assert!(store
        .find_root(msgid(5_000).ulid())
        .await
        .unwrap()
        .is_none());
    assert!(store.is_deleted(&chan, msgid(4_000).ulid()).await.unwrap());
    assert!(!store.is_deleted(&chan, msgid(2_000).ulid()).await.unwrap());

    // Materialization through a real fetch round trip.
    let roots = store.roots(&chan, page(10)).await.unwrap();
    let ulids: Vec<Ulid> = roots.iter().map(|r| r.msgid.ulid()).collect();
    let children = store.children(&chan, &ulids).await.unwrap();
    let items = materialize(roots, children);
    assert_eq!(items.len(), 4);
    assert!(
        matches!(&items[1], HistoryItem::Message { body, edited: Some((1, 5_000)), .. } if body == "m2000 final")
    );
    assert!(matches!(&items[3], HistoryItem::Tombstone { .. }));

    // -- purge + watermark --
    assert_eq!(store.purged_before(&chan).await.unwrap(), None);
    assert_eq!(store.purge_before(&chan, 2_500).await.unwrap(), 2);
    assert_eq!(store.purged_before(&chan).await.unwrap(), Some(2_500));
    let remaining = store.roots(&chan, page(10)).await.unwrap();
    assert_eq!(remaining.len(), 2);
    // The old root's edit (at 5000, newer than cutoff) died with its root.
    assert!(store
        .children(&chan, &[msgid(2_000).ulid()])
        .await
        .unwrap()
        .is_empty());
    // Watermark never regresses.
    store.purge_before(&chan, 1_000).await.unwrap();
    assert_eq!(store.purged_before(&chan).await.unwrap(), Some(2_500));

    // -- DM scopes + global DM purge --
    let dm = Scope::dm(ada.clone(), bob.clone());
    store.append(message(&dm, 1_000, "old dm")).await.unwrap();
    store.append(message(&dm, 9_000, "new dm")).await.unwrap();
    assert_eq!(store.purge_dms_before(5_000).await.unwrap(), 1);
    assert_eq!(store.roots(&dm, page(10)).await.unwrap().len(), 1);
    assert_eq!(store.purged_before(&dm).await.unwrap(), Some(5_000));

    // -- compaction: storage rewrite, invisible to materialization --
    let compact_chan: Scope = Scope::Channel(format!("#compact-{tag}").parse().unwrap());
    store
        .append(message(&compact_chan, 1_000, "v0"))
        .await
        .unwrap();
    for (at, body) in [(2_000, "v1"), (3_000, "v2"), (4_000, "v3")] {
        store
            .append(record(
                &compact_chan,
                at,
                1_000,
                "ada",
                EventKind::Edit { body: body.into() },
            ))
            .await
            .unwrap();
    }
    // Cancelled reaction pair, both old.
    store
        .append(record(
            &compact_chan,
            5_000,
            1_000,
            "bob",
            EventKind::React {
                emoji: "🔥".into(),
                add: true,
            },
        ))
        .await
        .unwrap();
    store
        .append(record(
            &compact_chan,
            6_000,
            1_000,
            "bob",
            EventKind::React {
                emoji: "🔥".into(),
                add: false,
            },
        ))
        .await
        .unwrap();
    // Global compaction: this scope contributes 2 superseded edits + a
    // cancelled pair; other scopes (this run's or, on a persistent
    // database, earlier runs') may add more — assert the floor only and
    // verify this scope's exact storage state below.
    let dropped = store.compact_before(10_000).await.unwrap();
    assert!(dropped >= 4, "expected at least 4 drops, got {dropped}");
    // Post-compaction, the materialized view is unchanged in substance.
    let roots = store.roots(&compact_chan, page(10)).await.unwrap();
    let ulids: Vec<Ulid> = roots.iter().map(|r| r.msgid.ulid()).collect();
    let children = store.children(&compact_chan, &ulids).await.unwrap();
    assert_eq!(children.len(), 1, "final edit only survives in storage");
    let items = materialize(roots, children);
    assert!(
        matches!(&items[0], HistoryItem::Message { body, edited: Some((1, 4_000)), reactions, .. }
            if body == "v3" && reactions.is_empty()),
        "compacted family must materialize to the same final state: {items:?}"
    );

    // -- accounts, devices, marks --
    assert!(store.register(&ada, "phc-string-ada").await.unwrap());
    assert!(!store.register(&ada, "phc-other").await.unwrap());
    assert_eq!(
        store.password_phc(&ada).await.unwrap().as_deref(),
        Some("phc-string-ada")
    );
    assert_eq!(store.password_phc(&bob).await.unwrap(), None);

    let device = [7u8; 32];
    assert!(!store.device_enrolled(&ada, &device).await.unwrap());
    assert!(store.enroll_device(&ada, device).await.unwrap());
    assert!(store.enroll_device(&ada, device).await.unwrap()); // idempotent
    assert!(store.device_enrolled(&ada, &device).await.unwrap());
    assert!(!store.enroll_device(&bob, device).await.unwrap()); // unknown account

    store
        .set_mark(&ada, "#general", &msgid(3_000))
        .await
        .unwrap();
    store
        .set_mark(&ada, "#general", &msgid(4_000))
        .await
        .unwrap(); // overwrite
    store.set_mark(&ada, "#dev", &msgid(1_000)).await.unwrap();
    let mut marks = store.marks(&ada).await.unwrap();
    marks.sort();
    assert_eq!(
        marks,
        vec![
            ("#dev".to_string(), msgid(1_000)),
            ("#general".to_string(), msgid(4_000)),
        ]
    );

    // -- verification claims (infrastructure only) --
    store
        .upsert_verification(&ada, "email", "ada@example.org")
        .await
        .unwrap();
    let pending = store.verifications(&ada).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, "email");
    assert_eq!(pending[0].verified_at, None, "claims start pending");

    assert!(store
        .confirm_verification(&ada, "email", 1_234)
        .await
        .unwrap());
    assert!(!store
        .confirm_verification(&ada, "age", 1_234)
        .await
        .unwrap()); // no claim
    let confirmed = store.verifications(&ada).await.unwrap();
    assert_eq!(confirmed[0].verified_at, Some(1_234));

    // Re-claiming a kind resets it to pending (e.g. changed email).
    store
        .upsert_verification(&ada, "email", "new@example.org")
        .await
        .unwrap();
    let reset = store.verifications(&ada).await.unwrap();
    assert_eq!(reset[0].subject, "new@example.org");
    assert_eq!(reset[0].verified_at, None);

    // -- channels: seed + load (the boot path) --
    let name: weft_proto::ChannelName = format!("#chan-{tag}").parse().unwrap();
    store
        .upsert_channel(&name, RetentionPolicy::Ephemeral)
        .await
        .unwrap();
    store
        .upsert_channel(&name, "retained:7d".parse().unwrap())
        .await
        .unwrap(); // policy update wins
    let channels = store.list_channels().await.unwrap();
    let found = channels.iter().find(|(n, _)| n == &name).unwrap();
    assert_eq!(found.1.to_string(), "retained:7d");

    // -- channel metadata + delete (§6.3) --
    store.set_channel_topic(&name, "the topic").await.unwrap();
    store.set_channel_view_gated(&name, true).await.unwrap();
    let record = store.channel(&name).await.unwrap().unwrap();
    assert_eq!(record.topic.as_deref(), Some("the topic"));
    assert!(record.view_gated);
    assert_eq!(record.policy.to_string(), "retained:7d");
    assert!(store.delete_channel(&name).await.unwrap());
    assert!(!store.delete_channel(&name).await.unwrap()); // idempotent-ish
    assert!(store.channel(&name).await.unwrap().is_none());

    // -- capability grants + revocation epochs (§6.5, §10.4) --
    let subj = format!("ada-{tag}");
    let scope = format!("#grants-{tag}");
    store
        .record_grant(&subj, &scope, &["ban".into(), "kick".into()], 0, Some(9999))
        .await
        .unwrap();
    let grants = store.grants_for(&subj).await.unwrap();
    let g = grants.iter().find(|g| g.scope == scope).unwrap();
    assert_eq!(g.caps, vec!["ban".to_string(), "kick".to_string()]);
    assert_eq!(g.expiry, Some(9999));
    // Re-grant replaces.
    store
        .record_grant(&subj, &scope, &["ban".into()], 1, None)
        .await
        .unwrap();
    let g = store
        .grants_for(&subj)
        .await
        .unwrap()
        .into_iter()
        .find(|g| g.scope == scope)
        .unwrap();
    assert_eq!(g.caps, vec!["ban".to_string()]);
    assert_eq!(g.epoch, 1);
    // Partial revoke of a cap not held is a no-op; revoking the held cap
    // drops the whole grant.
    assert_eq!(
        store
            .revoke_grants(&subj, &scope, Some(&["kick".into()]))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .revoke_grants(&subj, &scope, Some(&["ban".into()]))
            .await
            .unwrap(),
        1
    );
    assert!(store
        .grants_for(&subj)
        .await
        .unwrap()
        .iter()
        .all(|g| g.scope != scope));

    // Epochs monotonic per scope.
    let escope = format!("ns:epoch-{tag}");
    assert_eq!(store.scope_epoch(&escope).await.unwrap(), 0);
    assert_eq!(store.bump_epoch(&escope).await.unwrap(), 1);
    assert_eq!(store.bump_epoch(&escope).await.unwrap(), 2);
    assert_eq!(store.scope_epoch(&escope).await.unwrap(), 2);

    // -- invites: counter + expiry, atomic redeem (§6.5) --
    let limited = format!("inv-limited-{tag}");
    store
        .create_invite(InviteRecord {
            id: limited.clone(),
            scope: format!("ns:club-{tag}"),
            caps: vec!["view".into(), "send".into()],
            uses_left: Some(2),
            expiry: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        store.redeem_invite(&limited, 100).await.unwrap(),
        RedeemOutcome::Redeemed(_)
    ));
    assert!(matches!(
        store.redeem_invite(&limited, 100).await.unwrap(),
        RedeemOutcome::Redeemed(_)
    ));
    assert_eq!(
        store.redeem_invite(&limited, 100).await.unwrap(),
        RedeemOutcome::Exhausted
    );
    // Expired invite reads Gone.
    let expiring = format!("inv-exp-{tag}");
    store
        .create_invite(InviteRecord {
            id: expiring.clone(),
            scope: "#x".into(),
            caps: vec!["view".into()],
            uses_left: None,
            expiry: Some(500),
        })
        .await
        .unwrap();
    assert!(matches!(
        store.redeem_invite(&expiring, 499).await.unwrap(),
        RedeemOutcome::Redeemed(_)
    )); // unlimited, still valid
    assert_eq!(
        store.redeem_invite(&expiring, 500).await.unwrap(),
        RedeemOutcome::Gone
    );
    // Revoke removes it; unknown ids are Gone.
    assert!(store.revoke_invite(&limited).await.unwrap());
    assert_eq!(
        store.redeem_invite(&limited, 100).await.unwrap(),
        RedeemOutcome::Gone
    );
    assert_eq!(
        store.redeem_invite("no-such-invite", 100).await.unwrap(),
        RedeemOutcome::Gone
    );

    // -- namespaces (§2.1, §2.2) --
    let ns: weft_proto::NamespaceName = format!("gaming{tag}").parse().unwrap();
    assert!(store
        .create_namespace(NamespaceRecord {
            name: ns.clone(),
            owner: format!("owner-{tag}").parse().unwrap(),
            root_key: "B64ROOT==".into(),
            visibility: "unlisted".into(),
            title: None,
            description: None,
            icon: None,
        })
        .await
        .unwrap());
    // Name taken → false (CONFLICT).
    assert!(!store
        .create_namespace(NamespaceRecord {
            name: ns.clone(),
            owner: format!("someone-{tag}").parse().unwrap(),
            root_key: "OTHER==".into(),
            visibility: "public".into(),
            title: None,
            description: None,
            icon: None,
        })
        .await
        .unwrap());
    let record = store.namespace(&ns).await.unwrap().unwrap();
    assert_eq!(record.owner.as_str(), format!("owner-{tag}"));
    assert_eq!(record.root_key, "B64ROOT==");
    assert_eq!(
        store
            .namespaces_owned(&format!("owner-{tag}"))
            .await
            .unwrap(),
        1
    );

    // Meta + visibility.
    store
        .set_namespace_meta(&ns, "title", "The Lounge")
        .await
        .unwrap();
    store
        .set_namespace_meta(&ns, "icon", ":game:")
        .await
        .unwrap();
    store.set_namespace_visibility(&ns, "public").await.unwrap();
    let record = store.namespace(&ns).await.unwrap().unwrap();
    assert_eq!(record.title.as_deref(), Some("The Lounge"));
    assert_eq!(record.visibility, "public");

    // DISCOVER lists public namespaces, cursor-paginated.
    let page = store.list_public(None, 100).await.unwrap();
    assert!(page.iter().any(|n| n.name == ns));
    let after_all = store
        .list_public(Some(&format!("gaming{tag}")), 100)
        .await
        .unwrap();
    assert!(
        !after_all.iter().any(|n| n.name == ns),
        "cursor is exclusive"
    );

    // Unlisted/private namespaces never appear in DISCOVER.
    store
        .set_namespace_visibility(&ns, "private")
        .await
        .unwrap();
    let page = store.list_public(None, 100).await.unwrap();
    assert!(!page.iter().any(|n| n.name == ns));

    assert!(store.delete_namespace(&ns).await.unwrap());
    assert!(store.namespace(&ns).await.unwrap().is_none());

    // -- channel layout: categories + order within a namespace --
    let nsl = format!("layout{tag}");
    let c1: weft_proto::ChannelName = format!("#{nsl}/general").parse().unwrap();
    let c2: weft_proto::ChannelName = format!("#{nsl}/random").parse().unwrap();
    let c3: weft_proto::ChannelName = format!("#{nsl}/voice").parse().unwrap();
    for c in [&c1, &c2, &c3] {
        store
            .upsert_channel(c, RetentionPolicy::Permanent)
            .await
            .unwrap();
    }
    // general: text/0, random: text/1, voice: (no category)/0
    store
        .set_channel_layout(&c1, Some("text"), 0)
        .await
        .unwrap();
    store
        .set_channel_layout(&c2, Some("text"), 1)
        .await
        .unwrap();
    store.set_channel_layout(&c3, None, 0).await.unwrap();
    let ordered = store.channels_in_namespace(&nsl).await.unwrap();
    let names: Vec<String> = ordered.iter().map(|(n, _)| n.to_string()).collect();
    // Uncategorized (voice) sorts first (NULL category), then text by position.
    assert_eq!(
        names,
        vec![
            format!("#{nsl}/voice"),
            format!("#{nsl}/general"),
            format!("#{nsl}/random")
        ]
    );
    assert_eq!(ordered[1].1.category.as_deref(), Some("text"));
    assert_eq!(ordered[1].1.position, 0);
}

#[tokio::test]
async fn memory_backend_contract() {
    suite(&MemoryStore::default(), "mem").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_backend_contract() {
    let Ok(url) = std::env::var("WEFT_TEST_DATABASE_URL") else {
        eprintln!("WEFT_TEST_DATABASE_URL not set — Postgres contract test skipped");
        return;
    };
    let store = weft_store::PgStore::connect(&url).await.expect("connect");
    // Unique tag per run: a persistent database must not collide with
    // earlier runs (no Date::now — the ULID crate provides entropy).
    let tag = format!("pg{}", Ulid::new().to_string().to_lowercase());
    suite(&store, &tag).await;
}
