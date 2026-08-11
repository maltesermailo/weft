//! The Postgres store contract: what a daemon writes, a restarted daemon
//! reads back identically. Gates on `WEFT_TEST_DATABASE_URL` like weft-store's
//! backend suite — tables are `matrix_`-prefixed, so sharing the weftd test
//! database is safe.

use weft_matrix::store::{Reaction, Room, Space, Store};

#[tokio::test]
async fn everything_written_survives_a_reconnect() {
    let Ok(url) = std::env::var("WEFT_TEST_DATABASE_URL") else {
        eprintln!("WEFT_TEST_DATABASE_URL not set — Postgres store test skipped");
        return;
    };

    let mut store = Store::connect(&url).await.expect("connect");
    // A clean slate: this test owns the matrix_* tables for its run.
    sqlx::raw_sql(
        "TRUNCATE matrix_spaces, matrix_rooms, matrix_member_rooms, matrix_links, \
         matrix_users, matrix_reactions, matrix_sent_reactions, matrix_bans, \
         matrix_projections, matrix_projected_rooms, matrix_projected_members",
    )
    .execute(store.pool().expect("connected store has a pool"))
    .await
    .expect("truncate");

    // One of everything, through the write-through mutators.
    let mut space = Space {
        ns_id: "nsid".into(),
        room_id: "!space:kde.org".into(),
        uri: "matrix://kde.org/community".into(),
        ..Space::default()
    };
    space.rooms.insert(
        "!gen:kde.org".into(),
        Room {
            chan_id: "chanid".into(),
            channel: "#nsid/chanid".into(),
            uri: "matrix://kde.org/community/chanid".into(),
        },
    );
    space.member_joined("carol@kde.org", "!gen:kde.org");
    store.save_space(space).await;

    store.link("$ev1", "kde.org/01abc", "!gen:kde.org").await;
    store
        .note_user(
            "01arz3ndektsv4rrffq69g5fav",
            "ada",
            "weft_01arz3ndektsv4rrffq69g5fav",
        )
        .await;
    store
        .reaction_add(
            "$r1",
            Reaction {
                root: "kde.org/01abc".into(),
                key: "👍".into(),
                by: "carol@kde.org".into(),
            },
        )
        .await;
    let sent = Reaction {
        root: "kde.org/01abc".into(),
        key: "🔥|weird|key".into(), // `|` is legal in an annotation key
        by: "ada@test.example".into(),
    };
    store.sent_note(sent.clone(), "$r2".into()).await;
    store
        .apply_bridging(&weft_proto::Event::Bridging {
            namespace: "01bx5zzkbkactav9wevgemmvrz".parse().unwrap(),
            state: weft_proto::BridgingState::Banned,
        })
        .await;

    // "Restart": a fresh connection must see the identical world.
    let mut restored = Store::connect(&url).await.expect("reconnect");

    let space = restored
        .state
        .spaces
        .get("matrix://kde.org/community")
        .expect("space");
    assert_eq!(space.ns_id, "nsid");
    assert_eq!(space.rooms["!gen:kde.org"].channel, "#nsid/chanid");
    assert!(space.member_rooms["carol@kde.org"].contains("!gen:kde.org"));

    assert_eq!(restored.state.links.msgid_of("$ev1"), Some("kde.org/01abc"));
    let at = restored.state.links.event_of("kde.org/01abc").expect("ref");
    assert_eq!(
        (at.room.as_str(), at.event.as_str()),
        ("!gen:kde.org", "$ev1")
    );

    let (ulid, user) = restored.state.users.by_account("ada").expect("user");
    assert_eq!(ulid, "01arz3ndektsv4rrffq69g5fav");
    assert_eq!(user.localpart, "weft_01arz3ndektsv4rrffq69g5fav");

    assert_eq!(restored.state.reactions["$r1"].by, "carol@kde.org");
    assert_eq!(
        restored.state.sent_reactions.take(&sent).as_deref(),
        Some("$r2"),
        "the structured reaction key survives the trip"
    );
    assert!(restored.state.bans.is_banned("01bx5zzkbkactav9wevgemmvrz"));

    // A membership delta persisted on its own (not via save_space) also lands.
    let uri = "matrix://kde.org/community".to_string();
    restored
        .state
        .spaces
        .get_mut(&uri)
        .unwrap()
        .member_left("carol@kde.org", "!gen:kde.org");
    restored
        .persist_member_room(&uri, "carol@kde.org", "!gen:kde.org", false)
        .await;

    let third = Store::connect(&url).await.expect("third connect");
    assert!(
        third.state.spaces[&uri].member_rooms.is_empty(),
        "the leave delta persisted"
    );

    // The **projected** roster, the outbound twin of the above. This is the one
    // that used to be memory-only, so a restart dropped every Matrix member of a
    // projected server out of the WEFT roster (and their presence with it).
    let mut projecting = third;
    projecting
        .save_projection("projns", "!space:test.example")
        .await;
    projecting
        .save_projected_room("projns", "#projns/chanid", "!gen:test.example")
        .await;
    projecting
        .state
        .projections
        .get_mut("projns")
        .unwrap()
        .member_joined("carol@kde.org", "!gen:test.example");
    projecting
        .persist_projected_member("projns", "carol@kde.org", "!gen:test.example", true)
        .await;

    let fourth = Store::connect(&url).await.expect("fourth connect");
    assert!(
        fourth.state.projections["projns"].member_rooms["carol@kde.org"]
            .contains("!gen:test.example"),
        "a projected member survives the restart"
    );

    // And the leave half, so the set can shrink as well as grow.
    let mut leaving = fourth;
    leaving
        .state
        .projections
        .get_mut("projns")
        .unwrap()
        .member_left("carol@kde.org", "!gen:test.example");
    leaving
        .persist_projected_member("projns", "carol@kde.org", "!gen:test.example", false)
        .await;

    let fifth = Store::connect(&url).await.expect("fifth connect");
    assert!(
        fifth.state.projections["projns"].member_rooms.is_empty(),
        "the projected leave delta persisted"
    );
}
