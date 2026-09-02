//! Short links, against a real database.
//!
//! The two that carry this file are
//! [`shortening_the_same_key_twice_gives_the_same_link`] — the L8 property that
//! stops a retried reminder sending a customer two URLs — and
//! [`only_one_of_two_people_racing_for_a_single_use_link_gets_it`], which only
//! holds under concurrency and is where a check-then-update version would be
//! wrong.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use erp_links::{Link, LinkError, New, StoreError, follow, link, shorten};
use erp_testkit::{Schema, Template, TestDb};
use erp_types::Timestamp;

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

async fn tenant_db() -> TestDb {
    Template::get(&TENANT)
        .await
        .expect("tenant template builds")
        .fresh()
        .await
        .expect("tenant database clones")
}

fn at(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

fn reminder(key: &str) -> New {
    New {
        key: key.to_owned(),
        target: "/v1/booking/public/reservations/BK-1".to_owned(),
        external: false,
        expires_at: None,
        single_use: false,
        at: at("2026-05-01"),
    }
}

fn refusal(e: StoreError) -> LinkError {
    match e {
        StoreError::Refused(refused) => refused,
        StoreError::Database(e) => panic!("expected a refusal, got {e}"),
    }
}

/// **L8, for a thing somebody's phone is holding.**
///
/// A reminder that is retried must not text a second URL: the first one is
/// already out there and is the one that will be tapped.
#[tokio::test]
async fn shortening_the_same_key_twice_gives_the_same_link() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let first = shorten(&mut conn, &reminder("booking.reminder.BK-1"))
        .await
        .expect("shortens");
    let again = shorten(&mut conn, &reminder("booking.reminder.BK-1"))
        .await
        .expect("shortens again");

    assert_eq!(first, again, "a retry made a second link");

    // And a different key is a different link, or the idempotency would be
    // collapsing everything into one.
    let other = shorten(&mut conn, &reminder("booking.reminder.BK-2"))
        .await
        .expect("shortens");
    assert_ne!(first, other);
}

/// **The first target wins.**
///
/// A token already on somebody's phone must keep meaning what it meant, so
/// re-shortening a key with a different target does not move it. Repointing is
/// a new link, deliberately.
#[tokio::test]
async fn re_shortening_does_not_move_a_link_that_has_been_sent() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let token = shorten(&mut conn, &reminder("k")).await.expect("shortens");

    let moved = New {
        target: "/v1/booking/public/reservations/BK-999".to_owned(),
        ..reminder("k")
    };
    let same = shorten(&mut conn, &moved).await.expect("shortens");
    assert_eq!(token, same);

    let followed = follow(&mut conn, &token, at("2026-05-02"))
        .await
        .expect("follows");
    assert_eq!(followed.target, "/v1/booking/public/reservations/BK-1");
}

/// The tokens are not sequential, guessable, or derived from the key.
#[tokio::test]
async fn a_token_is_not_derived_from_the_key() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let mut tokens = Vec::new();
    for n in 0..20 {
        tokens.push(
            shorten(&mut conn, &reminder(&format!("booking.reminder.BK-{n}")))
                .await
                .expect("shortens"),
        );
    }

    for token in &tokens {
        assert_eq!(token.len(), 16, "a token is sixteen characters: {token}");
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "a token is hex so it survives being read aloud: {token}"
        );
    }

    let mut unique = tokens.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), tokens.len(), "two links share a token");

    // The keys were consecutive; the tokens must not be. Sorting the tokens and
    // finding them in insertion order would mean the database is handing out a
    // sequence with a hash on top.
    let mut sorted = tokens.clone();
    sorted.sort_unstable();
    assert_ne!(sorted, tokens, "the tokens are in insertion order");
}

/// Following one records that it was followed. **The whole visit record**: a
/// business asks "did they open it, and when", and that is three columns.
#[tokio::test]
async fn following_a_link_records_the_visit() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let token = shorten(&mut conn, &reminder("k")).await.expect("shortens");
    assert_eq!(unwrapped(link(&mut conn, &token).await).visits, 0);

    follow(&mut conn, &token, at("2026-05-02"))
        .await
        .expect("follows");
    follow(&mut conn, &token, at("2026-05-04"))
        .await
        .expect("follows again");

    let seen = unwrapped(link(&mut conn, &token).await);
    assert_eq!(seen.visits, 2);
    assert_eq!(seen.first_visit_at, Some(at("2026-05-02")));
    assert_eq!(seen.last_visit_at, Some(at("2026-05-04")));
}

/// An expired link says so, rather than saying it does not exist.
///
/// The difference matters to the person holding the phone: "ask for a new one"
/// and "check you copied it whole" are different instructions.
#[tokio::test]
async fn an_expired_link_says_it_expired() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let token = shorten(
        &mut conn,
        &New {
            expires_at: Some(at("2026-05-10")),
            ..reminder("k")
        },
    )
    .await
    .expect("shortens");

    follow(&mut conn, &token, at("2026-05-09"))
        .await
        .expect("still good the day before");

    let refused = follow(&mut conn, &token, at("2026-05-11"))
        .await
        .expect_err("expired");
    assert_eq!(refusal(refused), LinkError::Expired);

    let unknown = follow(&mut conn, "0123456789abcdef", at("2026-05-01"))
        .await
        .expect_err("no such link");
    assert_eq!(refusal(unknown), LinkError::NoSuchLink);
}

/// **Only one of two people racing for a single-use link gets it.**
///
/// The property that a check-then-update version would not have. Both
/// connections follow at the same instant; the `UPDATE ... WHERE visits = 0`
/// is what makes exactly one of them win.
#[tokio::test]
async fn only_one_of_two_people_racing_for_a_single_use_link_gets_it() {
    let db = tenant_db().await;
    let pool = db.pool().clone();

    let token = {
        let mut conn = pool.acquire().await.expect("connection");
        shorten(
            &mut conn,
            &New {
                single_use: true,
                ..reminder("k")
            },
        )
        .await
        .expect("shortens")
    };

    let one = {
        let pool = pool.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("connection");
            follow(&mut conn, &token, at("2026-05-02")).await
        })
    };
    let two = {
        let pool = pool.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("connection");
            follow(&mut conn, &token, at("2026-05-02")).await
        })
    };

    let results = [one.await.expect("joins"), two.await.expect("joins")];
    let winners = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(winners, 1, "a single-use link was used twice: {results:?}");

    // And the loser is told which of the three things happened.
    let loser = results
        .into_iter()
        .find_map(Result::err)
        .expect("one of them failed");
    assert_eq!(refusal(loser), LinkError::AlreadyUsed);
}

fn unwrapped(read: Result<Option<Link>, sqlx::Error>) -> Link {
    read.expect("reads").expect("the link exists")
}
