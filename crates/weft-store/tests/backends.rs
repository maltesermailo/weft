//! One suite, every backend: MemoryStore is the reference semantics and
//! PgStore must be indistinguishable through the traits. The Postgres run
//! gates on `WEFT_TEST_DATABASE_URL` (e.g.
//! `postgres://postgres:weft@127.0.0.1:15432/postgres`) and skips silently
//! when absent so `cargo test` needs no database.

use weft_proto::{
    Account, ChannelName, ContentState, MsgId, MsgMeta, NetworkName, ReportStatus, ResolveAction,
    RetentionPolicy, Ulid, UserRef,
};
use weft_store::{
    materialize, AccountStore, AuditStore, CapabilityStore, ChannelStore, EventKind, EventRecord,
    EventStore, HistoryItem, InviteRecord, InviteStore, MediaBlockRecord, MediaBlocklistStore,
    MediaStore, MembershipStore, MemoryStore, ModKind, ModRecord, ModerationStore, NamespaceRecord,
    NamespaceStore, NetblockRecord, NetblockStore, Page, PeerRecord, PeerStore, PendingRecovery,
    PinStore, ProfileStore, RedeemOutcome, ReportRecord, ReportResolution, ReportStore, RoleDef,
    RoleStore, Scope,
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
    S: EventStore
        + AccountStore
        + ChannelStore
        + CapabilityStore
        + InviteStore
        + NamespaceStore
        + ReportStore
        + PeerStore
        + NetblockStore
        + MediaBlocklistStore
        + ModerationStore
        + PinStore
        + MembershipStore
        + MediaStore
        + ProfileStore
        + RoleStore
        + AuditStore,
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

    // messages_by_sender (admin: every message a user authored, newest-first).
    // A dedicated scope + sender so it stays isolated from `chan`'s assertions.
    let poster = format!("poster-{tag}");
    let poster_ref = user(&poster).to_string();
    let pscope: Scope = Scope::Channel(format!("#poster-{tag}").parse().unwrap());
    for at in [10_000, 11_000] {
        store
            .append(record(
                &pscope,
                at,
                at,
                &poster,
                EventKind::Message {
                    body: format!("by {poster} at {at}"),
                    meta: MsgMeta::default(),
                },
            ))
            .await
            .unwrap();
    }
    let mine = store.messages_by_sender(&poster_ref, 10).await.unwrap();
    assert_eq!(mine.len(), 2, "only this sender's roots");
    assert_eq!(
        mine.iter().map(|r| r.at_ms()).collect::<Vec<_>>(),
        [11_000, 10_000],
        "newest-first"
    );

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
    // list_accounts (admin surface) includes registered accounts, sorted.
    assert!(store.list_accounts().await.unwrap().contains(&ada));

    // Account ULID (§10.4): minted at register, 26-char canonical, stable across
    // reads, unique per account, and absent for an unknown account.
    let ada_ulid = store
        .account_ulid(&ada)
        .await
        .unwrap()
        .expect("ada has a ULID");
    assert_eq!(ada_ulid.len(), 26, "canonical ULID is 26 chars");
    assert_eq!(
        store.account_ulid(&ada).await.unwrap().as_deref(),
        Some(ada_ulid.as_str()),
        "ULID is stable across reads"
    );
    assert!(
        store.account_ulid(&bob).await.unwrap().is_none(),
        "unknown account has no ULID"
    );
    let cara: Account = format!("cara-{tag}").parse().unwrap();
    assert!(store.register(&cara, "phc-cara").await.unwrap());
    assert_ne!(
        store.account_ulid(&cara).await.unwrap().unwrap(),
        ada_ulid,
        "distinct accounts get distinct ULIDs"
    );

    // delete_account (operator hard-delete) cascades per-account data but keeps
    // messages. Give `cara` a membership + grant + moderation record, delete her,
    // and assert all three are gone.
    let cara_ulid = store.account_ulid(&cara).await.unwrap().unwrap();
    let dchan: ChannelName = format!("#del-{tag}").parse().unwrap();
    store.set_membership(&cara, &dchan).await.unwrap();
    store
        .record_grant(&cara_ulid, "*", &["send".to_string()], 0, None)
        .await
        .unwrap();
    store
        .set_moderation(ModRecord {
            scope: "*".to_string(),
            account: cara.clone(),
            kind: ModKind::Mute,
            actor: "op".to_string(),
            reason: None,
            at_ms: 9_000,
        })
        .await
        .unwrap();
    assert!(store.delete_account(&cara).await.unwrap());
    assert!(!store.list_accounts().await.unwrap().contains(&cara));
    assert!(store.account_ulid(&cara).await.unwrap().is_none());
    assert!(store.memberships(&cara).await.unwrap().is_empty());
    assert!(store.grants_for(&cara_ulid).await.unwrap().is_empty());
    assert!(!store
        .is_moderated(&cara, &["*".to_string()], ModKind::Mute)
        .await
        .unwrap());
    // Deleting an unknown account is a no-op false.
    assert!(!store.delete_account(&cara).await.unwrap());

    // WC3 soft delete: schedule → pending → cancel restores → re-schedule →
    // `due_deletions` surfaces it once its window elapses → finalize hard-deletes.
    let dan: Account = format!("dan-{tag}").parse().unwrap();
    assert!(store.register(&dan, "phc-dan").await.unwrap());
    assert!(store.deletion_scheduled(&dan).await.unwrap().is_none());
    assert!(store.schedule_deletion(&dan, 100_000).await.unwrap());
    assert_eq!(store.deletion_scheduled(&dan).await.unwrap(), Some(100_000));
    assert!(!store.due_deletions(99_999).await.unwrap().contains(&dan)); // not yet
                                                                         // Restore clears it; a second cancel is a no-op false.
    assert!(store.cancel_deletion(&dan).await.unwrap());
    assert!(store.deletion_scheduled(&dan).await.unwrap().is_none());
    assert!(!store.cancel_deletion(&dan).await.unwrap());
    // Re-schedule in the past → due, and the finalize (hard delete) works.
    assert!(store.schedule_deletion(&dan, 50_000).await.unwrap());
    assert!(store.due_deletions(60_000).await.unwrap().contains(&dan));
    assert!(!store.schedule_deletion(&cara, 1).await.unwrap()); // unknown → false
    assert!(store.delete_account(&dan).await.unwrap());
    assert!(!store.list_accounts().await.unwrap().contains(&dan));

    let device = [7u8; 32];
    assert!(!store.device_enrolled(&ada, &device).await.unwrap());
    assert!(store.enroll_device(&ada, device).await.unwrap());
    assert!(store.enroll_device(&ada, device).await.unwrap()); // idempotent
    assert!(store.device_enrolled(&ada, &device).await.unwrap());
    assert!(!store.enroll_device(&bob, device).await.unwrap()); // unknown account
                                                                // Device list (WC4): enrolled pubkeys; unknown account is empty.
    assert_eq!(store.devices(&ada).await.unwrap(), vec![device]);
    assert!(store.devices(&bob).await.unwrap().is_empty());

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

    // accounts_by_email_domain (WC4 "find related"): case-insensitive match on
    // the part after `@`, verified or pending. Tag-unique domain keeps the PG
    // re-run isolated.
    let domain = format!("mail-{tag}.test");
    store
        .upsert_verification(&ada, "email", &format!("ada@{domain}"))
        .await
        .unwrap();
    let eve: Account = format!("eve-{tag}").parse().unwrap();
    let frank: Account = format!("frank-{tag}").parse().unwrap();
    store.register(&eve, "phc-eve").await.unwrap();
    store.register(&frank, "phc-frank").await.unwrap();
    store
        .upsert_verification(&eve, "email", &format!("eve@{domain}"))
        .await
        .unwrap();
    store
        .upsert_verification(&frank, "email", &format!("frank@other-{tag}.test"))
        .await
        .unwrap();
    // Uppercase query proves case-insensitivity; `frank` (other domain) excluded.
    let related = store
        .accounts_by_email_domain(&domain.to_uppercase())
        .await
        .unwrap();
    assert_eq!(related, vec![ada.clone(), eve.clone()]);

    // -- channels: seed + load (the boot path) --
    let name: weft_proto::ChannelName = format!("#chan-{tag}").parse().unwrap();
    store
        .upsert_channel(
            &name,
            RetentionPolicy::Ephemeral,
            weft_proto::ChannelKind::Text,
        )
        .await
        .unwrap();
    store
        .upsert_channel(
            &name,
            "retained:7d".parse().unwrap(),
            weft_proto::ChannelKind::Text,
        )
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
            recovery_set: None,
            pending_recovery: None,
            categories: Vec::new(),
            federation: false,
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
            recovery_set: None,
            pending_recovery: None,
            categories: Vec::new(),
            federation: false,
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

    // §11.10 auto-federation flag toggles + persists (default closed).
    assert!(!record.federation);
    store.set_namespace_federation(&ns, true).await.unwrap();
    assert!(store.namespace(&ns).await.unwrap().unwrap().federation);
    store.set_namespace_federation(&ns, false).await.unwrap();
    assert!(!store.namespace(&ns).await.unwrap().unwrap().federation);
    store.set_namespace_federation(&ns, true).await.unwrap();

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
            .upsert_channel(c, RetentionPolicy::Permanent, weft_proto::ChannelKind::Text)
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

    // -- recovery ladder state (§2.4) --
    let rns: weft_proto::NamespaceName = format!("recov{tag}").parse().unwrap();
    store
        .create_namespace(NamespaceRecord {
            name: rns.clone(),
            owner: format!("owner-{tag}").parse().unwrap(),
            root_key: "ROOT1==".into(),
            visibility: "unlisted".into(),
            title: None,
            description: None,
            icon: None,
            recovery_set: None,
            pending_recovery: None,
            categories: Vec::new(),
            federation: false,
        })
        .await
        .unwrap();
    // Designate a 2-of-3 quorum.
    store
        .set_recovery_set(&rns, 2, &["K1==".into(), "K2==".into(), "K3==".into()])
        .await
        .unwrap();
    let rec = store.namespace(&rns).await.unwrap().unwrap();
    assert_eq!(
        rec.recovery_set,
        Some((2, vec!["K1==".into(), "K2==".into(), "K3==".into()]))
    );

    // Start a pending recovery with a future eta; it isn't due yet.
    store
        .set_pending_recovery(
            &rns,
            PendingRecovery {
                new_root_key: "ROOT2==".into(),
                new_owner: format!("carol-{tag}"),
                eta_ms: 10_000,
                rung: 2,
            },
        )
        .await
        .unwrap();
    assert!(store
        .due_recoveries(9_999)
        .await
        .unwrap()
        .iter()
        .all(|n| n.name != rns));
    assert!(store
        .due_recoveries(10_000)
        .await
        .unwrap()
        .iter()
        .any(|n| n.name == rns));

    // Cancel clears it.
    store.clear_pending_recovery(&rns).await.unwrap();
    assert!(store
        .namespace(&rns)
        .await
        .unwrap()
        .unwrap()
        .pending_recovery
        .is_none());

    // Apply a rotation: owner + root key change, root-history records it.
    store
        .rotate_root(&rns, &format!("carol-{tag}"), "ROOT2==", false, 10_000)
        .await
        .unwrap();
    let rec = store.namespace(&rns).await.unwrap().unwrap();
    assert_eq!(rec.owner.as_str(), format!("carol-{tag}"));
    assert_eq!(rec.root_key, "ROOT2==");
    // A rung-3 rotation is marked operator-initiated forever.
    store
        .rotate_root(&rns, &format!("dave-{tag}"), "ROOT3==", true, 20_000)
        .await
        .unwrap();
    let history = store.root_history(&rns).await.unwrap();
    assert_eq!(history.len(), 2);
    assert!(!history[0].operator_initiated);
    assert!(history[1].operator_initiated);
    assert_eq!(history[1].root_key, "ROOT3==");

    // -- reports + retention holds (§6.7, §12.1, invariant 11) --
    let rep_chan: Scope = Scope::Channel(format!("#rep-{tag}").parse().unwrap());
    for at in 100_000..=100_010 {
        store
            .append(message(&rep_chan, at, &format!("r{at}")))
            .await
            .unwrap();
    }
    let reported = msgid(100_005);
    let report = |id: &str, mid: &MsgId, state, note: Option<&str>, queues: &[&str]| ReportRecord {
        id: id.to_string(),
        msgid: mid.clone(),
        scope: rep_chan.clone(),
        category: "harassment".into(),
        state,
        reporter: ada.clone(),
        note: note.map(str::to_string),
        queue_scopes: queues.iter().map(|s| s.to_string()).collect(),
        status: ReportStatus::Open,
        filed_at_ms: 500_000,
        held_roots: vec![],
        resolution: None,
        holds_released: false,
    };
    // Verified report places holds; ±HOLD_RADIUS covers all 11 roots here.
    store
        .file_report(report(
            &format!("{tag}rep1"),
            &reported,
            ContentState::Verified,
            Some("stop"),
            &[&format!("ns:{tag}"), "*"],
        ))
        .await
        .unwrap();
    let held = store.report(&format!("{tag}rep1")).await.unwrap().unwrap();
    assert_eq!(held.held_roots.len(), 11, "reported root + context held");
    // Invariant 11: held content is exempt from purge.
    assert_eq!(store.purge_before(&rep_chan, 200_000).await.unwrap(), 0);
    assert_eq!(
        store
            .roots(
                &rep_chan,
                Page {
                    before: None,
                    after: None,
                    limit: 50
                }
            )
            .await
            .unwrap()
            .len(),
        11
    );

    // Listed at both queue scopes; net-only query excludes an ns-only report.
    // `*` is a GLOBAL scope — a persistent database carries other runs' net
    // reports, so count only this run's (ids are tag-prefixed).
    let ns_scope = format!("ns:{tag}");
    let mine = |reports: Vec<ReportRecord>| {
        reports
            .into_iter()
            .filter(|r| r.id.starts_with(tag))
            .count()
    };
    assert_eq!(
        store
            .list_reports(&ns_scope, Some(ReportStatus::Open), None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        mine(store.list_reports("*", None, None, 100).await.unwrap()),
        1
    );

    // Unverified report holds nothing.
    store
        .file_report(report(
            &format!("{tag}rep2"),
            &msgid(100_006),
            ContentState::Unverified,
            None,
            &[&ns_scope],
        ))
        .await
        .unwrap();
    assert!(store
        .report(&format!("{tag}rep2"))
        .await
        .unwrap()
        .unwrap()
        .held_roots
        .is_empty());

    // Rate-limit counter sees both of ada's reports.
    assert_eq!(store.reports_by_since(&ada, 400_000).await.unwrap(), 2);
    assert_eq!(store.reports_by_since(&ada, 600_000).await.unwrap(), 0);

    // Escalate the ns-only report → net queue gains it.
    assert!(store.escalate_report(&format!("{tag}rep2")).await.unwrap());
    assert_eq!(
        mine(store.list_reports("*", None, None, 100).await.unwrap()),
        2
    );

    // Resolve rep1; holds persist until the grace window passes.
    let resolution = ReportResolution {
        action: ResolveAction::UserActioned,
        note: Some("banned".into()),
        resolved_by: bob.to_string(),
        at_ms: 700_000,
        hold_release_at: 700_000 + 7 * 24 * 3_600 * 1_000,
    };
    assert!(store
        .resolve_report(&format!("{tag}rep1"), resolution)
        .await
        .unwrap());
    // Double-resolve refused.
    assert!(!store
        .resolve_report(
            &format!("{tag}rep1"),
            ReportResolution {
                action: ResolveAction::Dismissed,
                note: None,
                resolved_by: bob.to_string(),
                at_ms: 700_001,
                hold_release_at: 700_001,
            }
        )
        .await
        .unwrap());
    // Before grace: still held.
    assert_eq!(store.release_due_holds(700_000).await.unwrap(), 0);
    assert_eq!(store.purge_before(&rep_chan, 200_000).await.unwrap(), 0);
    // After grace: holds released, content becomes purgeable.
    let after_grace = 700_000 + 8 * 24 * 3_600 * 1_000;
    assert_eq!(store.release_due_holds(after_grace).await.unwrap(), 1);
    assert_eq!(store.release_due_holds(after_grace).await.unwrap(), 0); // idempotent
    assert_eq!(store.purge_before(&rep_chan, 200_000).await.unwrap(), 11);
    assert!(store
        .roots(
            &rep_chan,
            Page {
                before: None,
                after: None,
                limit: 50
            }
        )
        .await
        .unwrap()
        .is_empty());

    // ---- §11 federation: peers + netblocks ----
    let peer: NetworkName = format!("peer-{tag}.example").parse().unwrap();
    assert!(store.peer(&peer).await.unwrap().is_none());

    // A fresh PROPOSE: manifest at v1, not yet acked.
    store
        .upsert_peer(PeerRecord {
            peer: peer.clone(),
            scope: "#general".to_string(),
            manifest: "MANIFEST_V1".to_string(),
            version: 1,
            acked_manifest: None,
            severed: false,
            created_ms: 1_000,
            updated_ms: 1_000,
        })
        .await
        .unwrap();
    let stored = store.peer(&peer).await.unwrap().unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.acked_manifest, None);

    // ACCEPT sets the acked manifest (upsert replaces).
    store
        .upsert_peer(PeerRecord {
            acked_manifest: Some("MANIFEST_V1".to_string()),
            updated_ms: 2_000,
            ..stored
        })
        .await
        .unwrap();
    assert_eq!(
        store.peer(&peer).await.unwrap().unwrap().acked_manifest,
        Some("MANIFEST_V1".to_string())
    );
    assert_eq!(store.list_peers().await.unwrap().len(), 1);
    assert!(store.remove_peer(&peer).await.unwrap());
    assert!(!store.remove_peer(&peer).await.unwrap());

    // Netblocks are name-keyed and idempotent.
    let evil: NetworkName = format!("evil-{tag}.example").parse().unwrap();
    assert!(!store.is_netblocked(&evil).await.unwrap());
    store
        .add_netblock(NetblockRecord {
            network: evil.clone(),
            reason: Some("spam".to_string()),
            added_ms: 5_000,
            actor: "op".to_string(),
        })
        .await
        .unwrap();
    assert!(store.is_netblocked(&evil).await.unwrap());
    // Re-adding refreshes rather than duplicating.
    store
        .add_netblock(NetblockRecord {
            network: evil.clone(),
            reason: Some("chronic abuse".to_string()),
            added_ms: 6_000,
            actor: "op".to_string(),
        })
        .await
        .unwrap();
    let blocks: Vec<_> = store
        .list_netblocks()
        .await
        .unwrap()
        .into_iter()
        .filter(|b| b.network == evil)
        .collect();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].reason.as_deref(), Some("chronic abuse"));
    assert!(store.remove_netblock(&evil).await.unwrap());
    assert!(!store.is_netblocked(&evil).await.unwrap());
    assert!(!store.remove_netblock(&evil).await.unwrap());

    // §13 media hash blocklist: content-addressed, idempotent, name-keyed on hash.
    let bad_hash = format!("b3-bad-{tag}");
    assert!(!store.is_hash_blocked(&bad_hash).await.unwrap());
    store
        .block_hash(MediaBlockRecord {
            hash: bad_hash.clone(),
            reason: Some("csam".to_string()),
            added_ms: 7_000,
            actor: "op".to_string(),
        })
        .await
        .unwrap();
    assert!(store.is_hash_blocked(&bad_hash).await.unwrap());
    // Re-blocking refreshes rather than duplicating.
    store
        .block_hash(MediaBlockRecord {
            hash: bad_hash.clone(),
            reason: Some("illegal".to_string()),
            added_ms: 8_000,
            actor: "op2".to_string(),
        })
        .await
        .unwrap();
    let hblocks: Vec<_> = store
        .list_blocked_hashes()
        .await
        .unwrap()
        .into_iter()
        .filter(|b| b.hash == bad_hash)
        .collect();
    assert_eq!(hblocks.len(), 1);
    assert_eq!(hblocks[0].reason.as_deref(), Some("illegal"));
    assert!(store.unblock_hash(&bad_hash).await.unwrap());
    assert!(!store.is_hash_blocked(&bad_hash).await.unwrap());
    assert!(!store.unblock_hash(&bad_hash).await.unwrap());

    // ---- WC1 admin audit trail: append is hash-chained, list is
    //      newest-first + filterable, and any tamper is recomputable-detectable ----
    let audit_op = format!("op-{tag}");
    let mut appended = Vec::new();
    for (action, target) in [
        ("moderation.ban", "#chan/bob"),
        ("account.delete", "carol"),
        ("netblock.add", "evil.example"),
    ] {
        let rec = store
            .append_audit(weft_store::AuditEntry {
                operator: audit_op.clone(),
                action: action.to_string(),
                target: target.to_string(),
                ts_ms: 1_700_000_000_000,
                payload_digest: format!("digest-{action}"),
            })
            .await
            .unwrap();
        appended.push(rec);
    }
    // Consecutive appends form a chain: monotonic seq, each prev_hash == the
    // predecessor's hash, and each hash recomputes from its own fields.
    for w in appended.windows(2) {
        assert_eq!(w[1].seq, w[0].seq + 1, "monotonic seq");
        assert_eq!(w[1].prev_hash, w[0].hash, "chain link");
    }
    for rec in &appended {
        let expected = weft_store::audit_hash(
            rec.seq,
            &rec.operator,
            &rec.action,
            &rec.target,
            rec.ts_ms,
            &rec.payload_digest,
            &rec.prev_hash,
        );
        assert_eq!(rec.hash, expected, "hash recomputes from fields");
    }
    // Listing is newest-first, scoped by this run's operator; action narrows it.
    let listed = store.list_audit(Some(&audit_op), None, 100).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].action, "netblock.add", "newest first");
    assert_eq!(listed[2].action, "moderation.ban", "oldest last");
    let filtered = store
        .list_audit(Some(&audit_op), Some("account.delete"), 100)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].target, "carol");
    // Tamper detection: flip a stored field and the hash no longer recomputes.
    let tampered = &appended[1];
    let recomputed = weft_store::audit_hash(
        tampered.seq,
        &tampered.operator,
        &tampered.action,
        "someone-else", // was "carol"
        tampered.ts_ms,
        &tampered.payload_digest,
        &tampered.prev_hash,
    );
    assert_ne!(recomputed, tampered.hash, "tamper breaks the chain hash");

    // ---- §6.7 moderation deny-list ----
    let bob: Account = format!("bob-{tag}").parse().unwrap();
    let chan_scope = format!("#suite-{tag}");
    let ns_scope = format!("ns:suite-{tag}");
    // Covering scopes a channel MSG checks against.
    let covering = vec![chan_scope.clone(), ns_scope.clone(), "*".to_string()];

    assert!(!store
        .is_moderated(&bob, &covering, ModKind::Mute)
        .await
        .unwrap());
    // A namespace-scope mute covers the channel (a namespace moderator).
    store
        .set_moderation(ModRecord {
            scope: ns_scope.clone(),
            account: bob.clone(),
            kind: ModKind::Mute,
            actor: "mod".to_string(),
            reason: Some("spam".to_string()),
            at_ms: 1_000,
        })
        .await
        .unwrap();
    assert!(store
        .is_moderated(&bob, &covering, ModKind::Mute)
        .await
        .unwrap());
    // A mute is not a ban.
    assert!(!store
        .is_moderated(&bob, &covering, ModKind::Ban)
        .await
        .unwrap());
    // Listing by the exact scope.
    let list = store.list_moderation(&ns_scope).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason.as_deref(), Some("spam"));
    // Clearing at the channel scope doesn't touch the ns-scope mute.
    assert!(!store
        .clear_moderation(&chan_scope, &bob, ModKind::Mute)
        .await
        .unwrap());
    assert!(store
        .is_moderated(&bob, &covering, ModKind::Mute)
        .await
        .unwrap());
    assert!(store
        .clear_moderation(&ns_scope, &bob, ModKind::Mute)
        .await
        .unwrap());
    assert!(!store
        .is_moderated(&bob, &covering, ModKind::Mute)
        .await
        .unwrap());

    // ---- §6.4 pinned messages ----
    let pin_chan: weft_proto::ChannelName = format!("#pins-{tag}").parse().unwrap();
    assert!(store.pins(&pin_chan).await.unwrap().is_empty());
    let (m1, m2) = (msgid(1_000), msgid(2_000));
    store.set_pin(&pin_chan, &m2, true).await.unwrap();
    store.set_pin(&pin_chan, &m1, true).await.unwrap();
    store.set_pin(&pin_chan, &m1, true).await.unwrap(); // idempotent
    assert_eq!(
        store.pins(&pin_chan).await.unwrap(),
        vec![m1.clone(), m2.clone()],
        "oldest-first by ULID"
    );
    store.set_pin(&pin_chan, &m1, false).await.unwrap();
    assert_eq!(store.pins(&pin_chan).await.unwrap(), vec![m2]);

    // ---- §6.3 persistent membership ----
    let acct: Account = format!("mem-{tag}").parse().unwrap();
    let mc1: weft_proto::ChannelName = format!("#m1-{tag}").parse().unwrap();
    let mc2: weft_proto::ChannelName = format!("#m2-{tag}").parse().unwrap();
    assert!(store.memberships(&acct).await.unwrap().is_empty());
    store.set_membership(&acct, &mc1).await.unwrap();
    store.set_membership(&acct, &mc2).await.unwrap();
    store.set_membership(&acct, &mc1).await.unwrap(); // idempotent
    let mut m = store.memberships(&acct).await.unwrap();
    m.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(m, vec![mc1.clone(), mc2.clone()]);
    store.clear_membership(&acct, &mc1).await.unwrap();
    assert_eq!(store.memberships(&acct).await.unwrap(), vec![mc2]);

    // ---- §6.5 role definitions ----
    let rscope = format!("ns:roles-{tag}");
    assert!(store.roles(&rscope).await.unwrap().is_empty());
    store
        .set_role(
            &rscope,
            "Moderator",
            "#e8b93d",
            &["mute".into(), "ban".into()],
        )
        .await
        .unwrap();
    store
        .set_role(&rscope, "Member", "#3ba55d", &["send".into()])
        .await
        .unwrap();
    // Upsert: same name replaces color + caps.
    store
        .set_role(
            &rscope,
            "Moderator",
            "#ff0000",
            &["mute".into(), "ban".into(), "kick".into()],
        )
        .await
        .unwrap();
    let roles = store.roles(&rscope).await.unwrap();
    assert!(roles.contains(&RoleDef {
        name: "Moderator".into(),
        color: "#ff0000".into(),
        caps: vec!["mute".into(), "ban".into(), "kick".into()],
    }));
    assert_eq!(roles.len(), 2);
    store.delete_role(&rscope, "Member").await.unwrap();
    let roles = store.roles(&rscope).await.unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].name, "Moderator");

    // ---- §6.5 explicit role membership (local name OR foreign account@network) ----
    let racct = format!("racct-{tag}");
    let foreign = format!("alice@peer-{tag}.example");
    assert!(store.roles_of(&rscope, &racct).await.unwrap().is_empty());
    store
        .assign_role(&rscope, "Moderator", &racct)
        .await
        .unwrap();
    store
        .assign_role(&rscope, "Moderator", &racct)
        .await
        .unwrap(); // idempotent
    store
        .assign_role(&rscope, "Moderator", &foreign)
        .await
        .unwrap(); // a federated holder
    assert_eq!(
        store.roles_of(&rscope, &racct).await.unwrap(),
        vec!["Moderator".to_string()]
    );
    assert_eq!(
        store.roles_of(&rscope, &foreign).await.unwrap(),
        vec!["Moderator".to_string()]
    );
    let mut members = store.role_members(&rscope, "Moderator").await.unwrap();
    members.sort();
    assert_eq!(members, vec![foreign.clone(), racct.clone()]); // alice@… < racct-…
    store
        .unassign_role(&rscope, "Moderator", &racct)
        .await
        .unwrap();
    store
        .unassign_role(&rscope, "Moderator", &foreign)
        .await
        .unwrap();
    assert!(store.roles_of(&rscope, &racct).await.unwrap().is_empty());
    // Deleting a role drops its assignments.
    store
        .assign_role(&rscope, "Moderator", &racct)
        .await
        .unwrap();
    store.delete_role(&rscope, "Moderator").await.unwrap();
    assert!(store.roles_of(&rscope, &racct).await.unwrap().is_empty());

    // -- CHANNEL RENAME: re-key everything scoped to the channel name --
    let old: ChannelName = format!("#rn-{tag}/old").parse().unwrap();
    let new: ChannelName = format!("#rn-{tag}/new").parse().unwrap();
    let old_scope = Scope::Channel(old.clone());
    let new_scope = Scope::Channel(new.clone());
    store
        .upsert_channel(
            &old,
            RetentionPolicy::Permanent,
            weft_proto::ChannelKind::Text,
        )
        .await
        .unwrap();
    store
        .append(message(&old_scope, 7_000, "history"))
        .await
        .unwrap();
    store
        .record_grant("rn-mod", &old.to_string(), &["send".into()], 0, None)
        .await
        .unwrap();
    store.set_membership(&ada, &old).await.unwrap();
    store
        .set_role(&old.to_string(), "Voice", "#fff", &["react".into()])
        .await
        .unwrap();

    assert!(store.rename_channel(&old, &new).await.unwrap());

    // The old identity is gone everywhere; the new one carries it all.
    assert!(store.channel(&old).await.unwrap().is_none());
    assert!(store.channel(&new).await.unwrap().is_some());
    let p10 = Page {
        before: None,
        after: None,
        limit: 10,
    };
    assert!(store.roots(&old_scope, p10).await.unwrap().is_empty());
    assert_eq!(store.roots(&new_scope, p10).await.unwrap().len(), 1);
    assert!(store
        .grants_at_scope(&old.to_string())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.grants_at_scope(&new.to_string()).await.unwrap().len(),
        1
    );
    let mships = store.memberships(&ada).await.unwrap();
    assert!(mships.contains(&new) && !mships.contains(&old));
    assert!(store.roles(&old.to_string()).await.unwrap().is_empty());
    assert_eq!(store.roles(&new.to_string()).await.unwrap().len(), 1);

    // Guards: absent old, or already-taken new → false, no mutation.
    assert!(!store.rename_channel(&old, &new).await.unwrap()); // old now absent
    store
        .upsert_channel(
            &old,
            RetentionPolicy::Ephemeral,
            weft_proto::ChannelKind::Text,
        )
        .await
        .unwrap();
    assert!(!store.rename_channel(&old, &new).await.unwrap()); // new taken

    // -- §13 media references + orphan tracking (M-media-1) --
    let mchan = Scope::Channel(format!("#media-{tag}").parse().unwrap());
    let m1 = msgid(10_000);
    let h_a = format!("{tag}-blob-a");
    let h_b = format!("{tag}-blob-b");
    // Two blobs uploaded at t=10_000; a message references only h_a.
    let blob = |hash: &str| weft_store::BlobRecord {
        hash: hash.to_string(),
        mime: "image/png".into(),
        bytes: 10,
        width: Some(64),
        height: Some(48),
        thumb: None,
        created_ms: 10_000,
    };
    store.record_blob(blob(&h_a)).await.unwrap();
    store.record_blob(blob(&h_b)).await.unwrap();
    // Metadata round-trips.
    assert_eq!(
        store.blob_meta(&h_a).await.unwrap().unwrap().width,
        Some(64)
    );
    assert!(store.blob_meta("no-such-blob").await.unwrap().is_none());
    store
        .add_refs(&mchan, &m1, std::slice::from_ref(&h_a))
        .await
        .unwrap();
    assert_eq!(store.blob_scopes(&h_a).await.unwrap(), vec![mchan.clone()]);
    assert!(store.blob_scopes(&h_b).await.unwrap().is_empty());

    // Grace: a just-uploaded blob is NOT an orphan before the cutoff passes it.
    assert!(!store.orphans(5_000).await.unwrap().contains(&h_b));
    // Past the grace cutoff, the unreferenced h_b is an orphan; h_a is not.
    let orphans = store.orphans(20_000).await.unwrap();
    assert!(orphans.contains(&h_b) && !orphans.contains(&h_a));

    // Deleting the message re-orphans h_a; forget_blob clears it from tracking.
    store.drop_refs(&m1).await.unwrap();
    assert!(store.blob_scopes(&h_a).await.unwrap().is_empty());
    assert!(store.orphans(20_000).await.unwrap().contains(&h_a));
    store.forget_blob(&h_a).await.unwrap();
    assert!(!store.orphans(20_000).await.unwrap().contains(&h_a));

    // Retention purge: refs by an old msgid (ULID t=1_000) drop before the cutoff.
    let m_old = msgid(1_000);
    store
        .add_refs(&mchan, &m_old, std::slice::from_ref(&h_b))
        .await
        .unwrap();
    store.drop_refs_before(&mchan, 5_000).await.unwrap();
    assert!(store.blob_scopes(&h_b).await.unwrap().is_empty());

    // -- §10.3 profiles: set, read, batch, last-writer-wins --
    let pa = format!("ada-{tag}");
    let pb = format!("bob-{tag}@peer.example"); // a federated handle
    assert!(store.profile(&pa).await.unwrap().is_none());
    store
        .set_profile(
            &pa,
            weft_store::ProfileRecord {
                display: Some("Ada".into()),
                avatar: Some("b3-ada".into()),
                updated: 100,
            },
        )
        .await
        .unwrap();
    store
        .set_profile(
            &pb,
            weft_store::ProfileRecord {
                display: None,
                avatar: Some("b3-bob".into()),
                updated: 100,
            },
        )
        .await
        .unwrap();
    let got = store.profile(&pa).await.unwrap().unwrap();
    assert_eq!(got.display.as_deref(), Some("Ada"));
    assert_eq!(got.avatar.as_deref(), Some("b3-ada"));
    // Replace (last-writer-wins) — a new avatar, display cleared.
    store
        .set_profile(
            &pa,
            weft_store::ProfileRecord {
                display: None,
                avatar: Some("b3-ada2".into()),
                updated: 200,
            },
        )
        .await
        .unwrap();
    let got = store.profile(&pa).await.unwrap().unwrap();
    assert_eq!(got.display, None);
    assert_eq!(got.avatar.as_deref(), Some("b3-ada2"));
    assert_eq!(got.updated, 200);
    // Batch fetch skips the absent account.
    let batch = store
        .profiles(&[pa.clone(), pb.clone(), format!("ghost-{tag}")])
        .await
        .unwrap();
    assert_eq!(batch.len(), 2);
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
