//! Retry an answer (#936): the last question with no answer is answered again in place.
//!
//! Before, when an answer failed midway the question stayed in the store with no answer; to ask again you had to resend it, so the same question was stored
//! twice, and the unanswered copy was replayed to the model as an "unanswered turn" in every later question. A retry carries the
//! id that the question was stored under: nothing is stored again, and the answer lands after it.
use super::*;
use axum::response::IntoResponse;

const TOOL: Reply = Reply::Tool("find_entities", r#"{"name":"Acme"}"#);
const QUESTION: &str = "What changed at Acme?";

/// Result of one request: the full SSE text, or on rejection the status code and `{error, code}`
enum Outcome {
    Streamed(String),
    Refused(u16, serde_json::Value),
}

async fn post(f: &Fx, body: serde_json::Value) -> anyhow::Result<Outcome> {
    let req: ChatReq = serde_json::from_value(body)?;
    match chat(
        State(f.state.clone()),
        AuthUser(f.user.clone()),
        Path(f.kb),
        Json(req),
    )
    .await
    {
        Ok(sse) => {
            let body =
                axum::body::to_bytes(sse.into_response().into_body(), 4 * 1024 * 1024).await?;
            Ok(Outcome::Streamed(
                String::from_utf8_lossy(&body).into_owned(),
            ))
        }
        Err(refused) => {
            let response = refused.into_response();
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
            Ok(Outcome::Refused(
                status,
                serde_json::from_slice(&body).unwrap_or_default(),
            ))
        }
    }
}

/// Messages stored for this conversation: (id, role, body), in time order
async fn stored(f: &Fx, conversation_id: Uuid) -> anyhow::Result<Vec<(Uuid, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, role, content FROM conversation_messages
          WHERE conversation_id = $1 ORDER BY created_at, id",
    )
    .bind(conversation_id)
    .fetch_all(&f.pool)
    .await?)
}

async fn only_conversation(f: &Fx) -> anyhow::Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT id FROM conversations WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&f.pool)
            .await?,
    )
}

/// Put a message straight into the store: the states after a failure and a follow-up question do not each need the model to play them out
async fn append(f: &Fx, conversation_id: Uuid, role: &str, content: &str) -> anyhow::Result<Uuid> {
    Ok(utopia_store::conversations::append_message(
        &f.pool,
        conversation_id,
        role,
        content,
        &utopia_store::conversations::TurnRecord::empty(),
    )
    .await?)
}

fn retry(conversation_id: Uuid, question_id: Uuid) -> serde_json::Value {
    json!({
        "conversation_id": conversation_id,
        "message": QUESTION,
        "retry_message_id": question_id,
    })
}

/// Answer again in place the question whose answer failed midway: the store still holds one question and one answer, and the model sees it only once
#[tokio::test]
async fn a_question_whose_answer_failed_is_answered_in_place() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![
        Reply::Http(401),
        TOOL,
        Reply::Text("Retried answer."),
    ]))
    .await?
    else {
        return Ok(());
    };
    let first = f.ask(QUESTION).await?;
    assert!(
        first.contains("event: error"),
        "the first answer fails:\n{first}"
    );
    let conversation_id = only_conversation(&f).await?;
    let before = stored(&f, conversation_id).await?;
    assert_eq!(before.len(), 1, "only the question is stored: {before:?}");
    let question_id = before[0].0;
    let asked_before = f.requests().len();

    let Outcome::Streamed(sse) = post(&f, retry(conversation_id, question_id)).await? else {
        panic!("the retry was refused");
    };
    assert!(sse.contains("event: done"), "{sse}");
    // The `conversation` frame carries the id stored for this question: the UI uses it to retry
    assert!(
        sse.contains(&format!("\"message_id\":\"{question_id}\"")),
        "{sse}"
    );
    // It is still the same question, followed by the answer; the question was not stored a second time
    let after = stored(&f, conversation_id).await?;
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(
        (after[0].0, after[0].1.as_str(), after[0].2.as_str()),
        (question_id, "user", QUESTION)
    );
    assert_eq!(
        (after[1].1.as_str(), after[1].2.as_str()),
        ("assistant", "Retried answer.")
    );
    // In the first model request of the retry, this question appears only once
    let request = &f.requests()[asked_before];
    let asked = request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["role"] == "user" && m["content"].as_str().is_some_and(|c| c.contains(QUESTION))
        })
        .count();
    assert_eq!(asked, 1, "{request}");
    f.cleanup().await
}

/// A question already answered is not answered again: 409, the model is not called, the store gains no extra row
#[tokio::test]
async fn an_answered_question_is_not_answered_again() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![TOOL, Reply::Text("First answer.")])).await? else {
        return Ok(());
    };
    let sse = f.ask(QUESTION).await?;
    assert!(sse.contains("event: done"), "{sse}");
    let conversation_id = only_conversation(&f).await?;
    let before = stored(&f, conversation_id).await?;
    let asked = f.requests().len();
    let Outcome::Refused(status, body) = post(&f, retry(conversation_id, before[0].0)).await?
    else {
        panic!("an answered question was answered again");
    };
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("retry_answered")),
        "{body}"
    );
    assert_eq!(
        stored(&f, conversation_id).await?,
        before,
        "nothing is stored"
    );
    assert_eq!(f.requests().len(), asked, "the model is not asked");
    f.cleanup().await
}

/// If someone asked a follow-up, this question is not retried in place either: the answer would land after the later turns
#[tokio::test]
async fn a_question_the_conversation_went_on_from_is_not_answered_again() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let conversation_id =
        utopia_store::conversations::create(&f.pool, f.kb, f.user.id, QUESTION).await?;
    let first = append(&f, conversation_id, "user", QUESTION).await?;
    append(&f, conversation_id, "user", "And at Globex?").await?;
    let before = stored(&f, conversation_id).await?;
    let Outcome::Refused(status, body) = post(&f, retry(conversation_id, first)).await? else {
        panic!("a question the conversation went on from was answered again");
    };
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("retry_answered")),
        "{body}"
    );
    assert_eq!(stored(&f, conversation_id).await?, before);
    assert!(f.requests().is_empty(), "the model is not asked");
    f.cleanup().await
}

/// An answer is already being written in this conversation: 409; once it ends, the same request can be answered
#[tokio::test]
async fn a_question_is_not_answered_twice_at_once() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![TOOL, Reply::Text("Retried answer.")])).await? else {
        return Ok(());
    };
    let conversation_id =
        utopia_store::conversations::create(&f.pool, f.kb, f.user.id, QUESTION).await?;
    let question = append(&f, conversation_id, "user", QUESTION).await?;
    let running = f.state.live.begin(conversation_id).await.unwrap();
    running.start().await;
    let Outcome::Refused(status, body) = post(&f, retry(conversation_id, question)).await? else {
        panic!("a retry started while an answer was running");
    };
    assert_eq!(
        (status, body["code"].as_str()),
        (409, Some("answer_running")),
        "{body}"
    );
    assert!(f.requests().is_empty(), "the model is not asked");
    assert_eq!(stored(&f, conversation_id).await?.len(), 1);

    drop(running);
    let Outcome::Streamed(sse) = post(&f, retry(conversation_id, question)).await? else {
        panic!("the retry was refused after the running answer finished");
    };
    assert!(sse.contains("event: done"), "{sse}");
    let after = stored(&f, conversation_id).await?;
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(after[0].0, question);
    f.cleanup().await
}

/// The retry must name a question in this conversation
#[tokio::test]
async fn a_retry_names_a_question_of_this_conversation() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let conversation_id =
        utopia_store::conversations::create(&f.pool, f.kb, f.user.id, QUESTION).await?;
    let question = append(&f, conversation_id, "user", QUESTION).await?;
    let answer = append(&f, conversation_id, "assistant", "An answer.").await?;
    let elsewhere =
        utopia_store::conversations::create(&f.pool, f.kb, f.user.id, "Another").await?;
    let foreign = append(&f, elsewhere, "user", "Another question").await?;
    for (body, status, code) in [
        (
            json!({ "message": QUESTION, "retry_message_id": question }),
            422,
            Some("retry_needs_conversation"),
        ),
        (
            retry(conversation_id, answer),
            422,
            Some("retry_not_question"),
        ),
        (retry(conversation_id, foreign), 404, None),
        (retry(conversation_id, Uuid::now_v7()), 404, None),
    ] {
        let Outcome::Refused(got, refusal) = post(&f, body.clone()).await? else {
            panic!("{body} was not refused");
        };
        assert_eq!(got, status, "{body}: {refusal}");
        if let Some(code) = code {
            assert_eq!(refusal["code"], code, "{body}");
        }
    }
    assert!(f.requests().is_empty(), "the model is not asked");
    assert_eq!(stored(&f, conversation_id).await?.len(), 2);
    assert_eq!(stored(&f, elsewhere).await?.len(), 1);
    f.cleanup().await
}

/// Each question hands its stored id to the UI: when an answer fails midway, the UI retries with it
#[tokio::test]
async fn every_question_tells_the_interface_its_stored_id() -> anyhow::Result<()> {
    let Some(f) = fixture(Scripted::new(vec![TOOL, Reply::Text("An answer.")])).await? else {
        return Ok(());
    };
    let sse = f.ask(QUESTION).await?;
    let conversation_id = only_conversation(&f).await?;
    let question = stored(&f, conversation_id).await?[0].0;
    assert!(
        sse.contains(&format!("\"message_id\":\"{question}\"")),
        "{sse}"
    );
    f.cleanup().await
}

/// While retry eligibility is still being read, another tab must not subscribe to this placeholder, which will be rejected later.
#[tokio::test]
async fn a_rejected_retry_does_not_publish_a_stream_to_another_tab() -> anyhow::Result<()> {
    let _held = HELD_DB_LOCKS.lock().await;
    let Some(f) = fixture(Scripted::new(vec![])).await? else {
        return Ok(());
    };
    let application_name = format!("retry-preparation-{}", f.kb.simple());
    let retry_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            (*f.pool.connect_options())
                .clone()
                .application_name(&application_name),
        )
        .await?;
    let mut retry_state = f.state.clone();
    retry_state.pool = retry_pool.clone();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let id = utopia_store::conversations::create(&f.pool, f.kb, f.user.id, QUESTION).await?;
        let question = append(&f, id, "user", QUESTION).await?;
        append(&f, id, "assistant", "Already answered.").await?;
        let before = stored(&f, id).await?;

        // Only delay the eligibility query; the permission check before it reads other tables, and begin has already taken the slot.
        let mut lock = f.pool.begin().await?;
        sqlx::query("LOCK TABLE conversation_messages IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await?;
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await?;
        let pending = chat(
            State(retry_state),
            AuthUser(f.user.clone()),
            Path(f.kb),
            Json(ChatReq {
                conversation_id: Some(id),
                message: QUESTION.into(),
                retry_message_id: Some(question),
            }),
        );
        tokio::pin!(pending);
        let waiting = async {
            while !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                  WHERE application_name = $1 AND $2 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(&application_name)
            .bind(blocker)
            .fetch_one(&f.pool)
            .await?
            {}
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            _ = &mut pending => anyhow::bail!("retry must wait for its qualification query"),
            result = waiting => result?,
        }
        // Go straight to the resubscribe boundary, so a single poll does not stop only at the HTTP permission query.
        let other_tab = f.state.live.attach(id);
        tokio::pin!(other_tab);
        anyhow::ensure!(futures_util::poll!(&mut other_tab).is_pending());

        lock.rollback().await?;
        let refusal = pending
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("an answered question was retried"))?
            .into_response();
        anyhow::ensure!(refusal.status().as_u16() == 409);
        let body = axum::body::to_bytes(refusal.into_body(), 65536).await?;
        anyhow::ensure!(
            serde_json::from_slice::<serde_json::Value>(&body)?["code"] == "retry_answered"
        );
        let other_tab = sse_from(other_tab.await);
        let body = axum::body::to_bytes(other_tab.into_response().into_body(), 65536).await?;
        let sse = String::from_utf8_lossy(&body);
        anyhow::ensure!(sse.contains("event: idle"), "{sse}");
        anyhow::ensure!(
            !sse.contains("event: snapshot") && !sse.contains("event: error"),
            "{sse}"
        );
        anyhow::ensure!(stored(&f, id).await? == before);
        anyhow::ensure!(f.requests().is_empty());
        drop(f.state.live.begin(id).await?);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    retry_pool.close().await;
    f.cleanup().await?;
    result??;
    Ok(())
}
