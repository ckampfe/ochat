// TODO:
// - [x] ability to "fork"/"branch" from a certain portion of the conversation, by
//       creating a new conversation up to a certain point
// - [ ] auto reload in dev
// - [x] log level via RUST_LOG
// - [x] request logging (with tower)
// - [ ] message search
// - [ ] conversation tagging
// - [x] config
// - [x] make database file configurable
// - [x] state debugging endpoint
// - [x] back button on `show`
// - [x] edit conversation names
// - [ ] delete messages
// - [x] delete conversations (show)
// - [ ] bulk delete conversations (index)
// - [ ] cmd+enter to send messages
// - [x] fix Option::take panic
// - [x] fix SSE 'Done' not getting sent, by actually working with NDJSON
// - [ ] xdg spec for app data
// - [x] list available local models (curl http://localhost:11434/api/tags)
// - [x] selectable models per conversation
// - [x] ticker to indicate that the model is thinking
// - [x] get rid of in-memory history, load all from db every time
// - [x] cache/store available models in db
// - [x] store model on conversation,
//       to persist it when switching between conversations

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Sse;
use axum::response::sse::Event;
use axum::routing::{delete, get, post, put};
use axum::{Form, Router};
use clap::Parser;
use futures::{AsyncBufReadExt, Stream, TryStreamExt};
use maud::{DOCTYPE, Markup, html};
use serde::{Deserialize, Serialize};
use sqlx::pool::PoolConnection;
use sqlx::{Acquire, Sqlite};
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use tokio_stream::StreamExt;
use tracing::{debug, error};

const FILLED_BLOCK: char = '\u{2588}';

#[derive(Deserialize, Clone, Debug)]
struct ChatResponse {
    // model: String,
    // created_at: String,
    response: String,
    done: bool,
}

#[derive(Clone)]
enum OllamaResponseMessage {
    More { response: String },
    Done,
}

async fn send_chat_message(
    client: reqwest::Client,
    messages: &[Message],
    model: String,
    ollama_tx: broadcast::Sender<OllamaResponseMessage>,
) -> anyhow::Result<()> {
    let mut prompt = String::new();

    for message in messages {
        prompt.push_str(&message.who.to_string());
        prompt.push_str(": ");
        prompt.push_str(&message.body);
        prompt.push('\n');
    }

    let body = HashMap::from([("model", model), ("prompt", prompt)]);

    tokio::spawn(async move {
        let resp = client
            .post("http://localhost:11434/api/generate")
            .json(&body)
            .send()
            .await
            .unwrap();

        let bytes_stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            .into_async_read();

        let reader = futures::io::BufReader::new(bytes_stream);

        let mut lines = reader.lines();

        while let Some(line) = lines.next().await {
            match line {
                Ok(line) => {
                    if let Ok(chat_response) = serde_json::from_str::<ChatResponse>(&line) {
                        if chat_response.done {
                            let _ = ollama_tx.send(OllamaResponseMessage::Done);
                            debug!("sent DONE to ollama_tx");
                        } else {
                            let _ = ollama_tx.send(OllamaResponseMessage::More {
                                response: chat_response.response,
                            });
                            debug!("sent More to ollama_tx");
                        }
                    } else {
                        // TODO we should be able to propagate some error response
                        // to the client here.
                        error!("could not deserialize chat chunk: {:?}", line);
                    }
                }
                Err(e) => {
                    // TODO we should be able to propagate some error response
                    // to the client here.
                    error!("error receiving ndjson stream: {:?}", e);
                }
            }
        }
    });

    Ok(())
}

#[derive(Deserialize, Debug)]
struct OllamaModelsResponse {
    models: Vec<OllamaModelResponse>,
}

#[derive(Deserialize, Debug)]
struct OllamaModelResponse {
    name: String,
}

async fn get_available_models(client: reqwest::Client) -> anyhow::Result<Vec<String>> {
    let models: OllamaModelsResponse = client
        .get("http://localhost:11434/api/tags")
        .send()
        .await?
        .json()
        .await?;

    let models = models.models.into_iter().map(|m| m.name).collect();

    Ok(models)
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct Model {
    id: i64,
    name: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct Message {
    id: i64,
    body: String,
    who: String,
    conversation_id: i64,
    inserted_at: String,
}

#[derive(sqlx::FromRow)]
struct Conversation {
    id: i64,
    name: String,
    model: String,
    source_conversation_id: Option<i64>,
    source_conversation_name: Option<String>,
    inserted_at: String,
    // updated_at: String,
}

#[derive(sqlx::FromRow)]
struct ConversationWithLastMessageTime {
    id: i64,
    name: String,
    source_conversation_id: Option<i64>,
    source_conversation_name: Option<String>,
    inserted_at: String,
    // updated_at: String,
    last_message_inserted_at: String,
}

async fn conversations_index(
    State(state): State<Arc<Mutex<AppState>>>,
) -> axum::response::Result<maud::Markup> {
    let state = state.lock().await;

    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    let conversations: Vec<ConversationWithLastMessageTime> = sqlx::query_as(
        "
        select
            conversations.id,
            conversations.name,
            c2.name as source_conversation_name,
            c2.id as source_conversation_id,
            conversations.inserted_at,
            -- conversations.updated_at,
            last_messages.last_message_inserted_at
        from conversations
        inner join (
            select 
                conversation_id,
                max(inserted_at) as last_message_inserted_at
            from messages
            group by conversation_id
        ) last_messages
            on last_messages.conversation_id = conversations.id
        left join conversations c2
            on conversations.source_conversation_id = c2.id
        order by conversations.inserted_at desc;
        ",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    Ok(html! {
        (DOCTYPE)
        head {
            meta charset="UTF-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title {
                "conversations"
            }
            // minified
            script
                src="https://unpkg.com/htmx.org@2.0.4"
                integrity="sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+"
                crossorigin="anonymous" {}
            // unminified
            // script
            //     src="https://unpkg.com/htmx.org@2.0.4/dist/htmx.js"
            //     integrity="sha384-oeUn82QNXPuVkGCkcrInrS1twIxKhkZiFfr2TdiuObZ3n3yIeMiqcRzkIcguaof1"
            //     crossorigin="anonymous" {}
            link
                rel="stylesheet"
                href="https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css";
            style {
                "
                pre {
                    white-space: pre-wrap;
                }
                "
            }
        }
        body {
            div class="container mb-5" {
                nav class="level" {
                    div class="level-left" {
                        div class="level-item" {
                            h2 class="subtitle" {
                                "conversations"
                            }
                        }

                        div class="level-item" {
                            a hx-post="/conversations/new" {
                                "new conversation"
                            }
                        }
                    }
                }
                table class="table container" {
                    thead {
                        tr {
                            th { "started" }
                            th { "last message" }
                            th { "name" }
                            th { "source" }
                        }
                    }

                    @for conversation in conversations {
                        tbody {
                            tr {
                                td {
                                    (conversation.inserted_at)
                                }
                                td {
                                    (conversation.last_message_inserted_at)
                                }
                                td {
                                    a href=(format!("/conversations/{}", conversation.id)) {
                                        (conversation.name)
                                    }
                                }
                                @if let Some(source_conversation_name) = conversation.source_conversation_name {
                                    td {
                                        a href=(format!("/conversations/{}", conversation.source_conversation_id.unwrap())) {
                                            (source_conversation_name)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

async fn conversations_show(
    State(state): State<Arc<Mutex<AppState>>>,
    Path(conversation_id): Path<i64>,
) -> axum::response::Result<maud::Markup> {
    let state = state.lock().await;

    let available_models = get_available_models(state.http_client.clone())
        .await
        .map_err(|e| e.to_string())?;

    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    drop(state);

    let mut txn = conn.begin().await.map_err(|e| e.to_string())?;

    // TODO determine if we want to do this every time we load a conversation
    for model in available_models {
        sqlx::query(
            "
        insert into models
        (name) values (?)
        on conflict do nothing;
        ",
        )
        .bind(model)
        .execute(&mut *txn)
        .await
        .map_err(|e| e.to_string())?;
    }

    let models: Vec<Model> = sqlx::query_as(
        "
        select
            id,
            name
        from models
        order by name;
        ",
    )
    .fetch_all(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    let conversation: Conversation = sqlx::query_as(
        "
    select
        conversations.id,
        conversations.name,
        models.name as model,
        conversations.source_conversation_id,
        c2.name as source_conversation_name,
        conversations.inserted_at
        -- conversations.updated_at
    from conversations
    left join conversations c2
        on conversations.source_conversation_id = c2.id
    inner join models
        on models.id = conversations.model_id
    where conversations.id = ?
    limit 1;
    ",
    )
    .bind(conversation_id)
    .fetch_one(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    let messages: Vec<Message> = sqlx::query_as(
        "
        select
            id,
            body,
            who,
            conversation_id,
            inserted_at
         from messages where conversation_id = ?;",
    )
    .bind(conversation_id)
    .fetch_all(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    txn.commit().await.map_err(|e| e.to_string())?;

    Ok(html! {
        (DOCTYPE)
        head {
            meta charset="UTF-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title {
                "conversations"
            }
            script
                src="https://unpkg.com/htmx.org@2.0.4"
                integrity="sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+"
                crossorigin="anonymous" {}
            script src="https://unpkg.com/htmx-ext-sse@2.2.2/sse.js" {}
            link
                rel="stylesheet"
                href="https://cdn.jsdelivr.net/npm/bulma@1.0.2/css/bulma.min.css";
            style {
                "
                pre {
                    white-space: pre-wrap;
                }
                "
            }
            script {
                (include_str!("htmx_ticker.js"))
            }
        }
        body {
            div class="container mb-5" {
                section class="section" {
                    a href="/conversations/" {
                        "Back"
                    }
                    div class="level" {
                        div
                            id="conversation-name-block"
                            class="level-left"
                        {
                            div class="level-item" {
                                h1 class="title" {
                                    (conversation.name)
                                }
                            }
                            div
                                id="conversation-name-edit"
                                class="level-item"
                            {
                                a
                                    hx-get=(format!("/conversations/{}/edit", conversation.id))
                                    hx-swap="outerHTML"
                                    hx-target="#conversation-name-edit"
                                {
                                    "Edit"
                                }
                            }
                        }
                    }

                    h2 class="subtitle" {
                        "Started: " (conversation.inserted_at)
                    }

                    @if let Some(last_message) = messages.last() {
                        h2 class="subtitle" {
                            "Last message at: " (last_message.inserted_at)
                        }
                    }

                    @if let Some(source_conversation_name) = conversation.source_conversation_name {
                        h2 class="subtitle" {
                            "Source: "
                            a href=(format!("/conversations/{}", conversation.source_conversation_id.unwrap())) {
                                (source_conversation_name)
                            }
                        }
                    }

                    a
                        hx-delete=(format!("/conversations/{}/delete", conversation.id))
                        hx-confirm="Really delete? Conversation and all messages will be destroyed."
                    {
                        "Delete conversation"
                    }

                    div {
                        select
                            name="model-id"
                            hx-post=(format!("/models/select/{}", conversation.id))
                            hx-swap="none"
                        {
                            @for model in models.iter() {
                                @if *model.name == conversation.model {
                                    option value=(model.id) selected {
                                        (model.name)
                                    }
                                } @else {
                                    option value=(model.id) {
                                        (model.name)
                                    }
                                }
                            }
                        }
                    }
                }

                table class="table container" {
                    thead {
                        tr {
                            th {
                                ""
                            }
                            th {
                                ""
                            }
                            th {
                                ""
                            }
                            th {
                                ""
                            }
                            th {
                                ""
                            }
                        }
                    }
                    tbody id="messages" {
                        @for (i, message) in messages.iter().enumerate() {
                            tr {
                                td {
                                    (i + 1)
                                }
                                td {
                                    (message.inserted_at)
                                }
                                td {
                                    (message.who.to_string())
                                }
                                td {
                                    pre {
                                        (message.body)
                                    }
                                }
                                td {
                                    a hx-post=(format!("/conversations/{}/fork/{}", conversation.id, message.id)) {
                                        "Fork"
                                    }
                                }
                            }
                        }
                    }
                }
                div {
                    form
                        id="chat-input-form"
                        hx-post="/messages/new"
                        hx-target="#messages"
                        hx-swap="beforeend"
                        // https://htmx.org/examples/keyboard-shortcuts/
                        // hx-trigger="keyup[metaKey&&key=='Enter'], keyup[shiftKey&&key=='Enter'] from:body"
                        hx-on::after-request="if(event.detail.successful) this.reset()"
                    {
                        div class="field" {
                            div class="control" {
                                textarea
                                    class="textarea"
                                    name="body" {}
                            }
                        }

                        input type="hidden" name="conversation_id" value=(conversation.id) {}
                        div class="field-body" {
                            div class="field is-grouped" {
                                div class="control" {
                                    button
                                        class="button is-link"
                                    {
                                        "Send"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

#[derive(Deserialize)]
struct MessageSendForm {
    body: String,
    conversation_id: i64,
}

async fn messages_create(
    State(state): State<Arc<Mutex<AppState>>>,
    Form(message_send_form): Form<MessageSendForm>,
) -> axum::response::Result<Markup> {
    let conversation_id = message_send_form.conversation_id;

    let state = state.lock().await;

    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;
    let conn2 = state.pool.acquire().await.map_err(|e| e.to_string())?;
    let http_client = state.http_client.clone();
    let ollama_tx = state.ollama_tx.clone();
    let ollama_rx2 = state.ollama_rx.resubscribe();

    drop(state);

    let mut txn = conn.begin().await.map_err(|e| e.to_string())?;

    let message: Message = sqlx::query_as(
        "
    insert into messages (
        who,
        body,
        conversation_id
    ) values (?, ?, ?)
    returning 
        id,
        body,
        who,
        conversation_id,
        inserted_at;",
    )
    .bind(Who::Me)
    .bind(message_send_form.body)
    .bind(conversation_id)
    .fetch_one(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    let messages: Vec<Message> = sqlx::query_as(
        "
        select 
            id,
            body,
            who,
            conversation_id,
            inserted_at
        from messages
        where conversation_id = ?
        order by inserted_at, id;
        ",
    )
    .bind(conversation_id)
    .fetch_all(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    // create the reply from llama.
    // initially, it's empty.
    let ollama_response: Message = sqlx::query_as(
        "
        insert into messages (
            who,
            body,
            conversation_id
        ) values (?, ?, ?)
         returning *;
         ",
    )
    .bind(Who::Llama)
    .bind("")
    .bind(conversation_id)
    .fetch_one(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    let model: Model = sqlx::query_as(
        "
    select
        models.id,
        models.name
    from models
    inner join conversations
        on conversations.model_id = models.id
    where conversations.id = ?
    limit 1;
    ",
    )
    .bind(conversation_id)
    .fetch_one(&mut *txn)
    .await
    .map_err(|e| e.to_string())?;

    txn.commit().await.map_err(|e| e.to_string())?;

    spawn_llm_response_update_task(conn2, ollama_response.id, ollama_rx2);

    send_chat_message(http_client, &messages, model.name, ollama_tx)
        .await
        .map_err(|e| e.to_string())?;

    let count = messages.len();

    Ok(html! {
            tr {
                td {
                    (count)
                }
                td {
                    (message.inserted_at)
                }
                td {
                    (message.who.to_string())
                }
                td {
                    pre {
                        (message.body)
                    }
                }
                td {
                    a hx-post=(format!("/conversations/{}/fork/{}", message.conversation_id, message.id)) {
                        "Fork"
                    }
                }
            }
            tr {
                td {
                    (count + 1)
                }
                td {
                    (ollama_response.inserted_at)
                }
                td {
                    (Who::Llama)
                }
                td {
                    // TODO
                    // document what this whole thing does...
                    div
                        id="sse-listener"
                        hx-ext="sse"
                        sse-connect="/messages/response/sse"
                        sse-swap="ChatData"
                        hx-target="next"
                        hx-swap="beforeend"
                    {
                        div
                        hx-get="/empty"
                        hx-trigger="sse:ChatDone"
                        hx-target="#sse-listener"
                        hx-swap="delete" {}
                    }
                    // javascript removes this id when the llm is done responding
                    pre id="llm-response" {
                        (FILLED_BLOCK)
                    }
                }
                td {
                    a hx-post=(format!("/conversations/{}/fork/{}", ollama_response.conversation_id, ollama_response.id)) {
                        "Fork"
                    }
                }
            }
    })
}

fn spawn_llm_response_update_task(
    mut conn: PoolConnection<Sqlite>,
    ollama_response_message_id: i64,
    mut ollama_rx: broadcast::Receiver<OllamaResponseMessage>,
) {
    tokio::spawn(async move {
        while let Ok(chat_chunk) = ollama_rx.recv().await {
            match chat_chunk {
                OllamaResponseMessage::More { response } => {
                    sqlx::query(
                        "
                        update messages
                        set body = body || ?
                         where id = ?
                         ",
                    )
                    .bind(response)
                    .bind(ollama_response_message_id)
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| e.to_string())
                    // TODO add some error channel here instead of unwrapping
                    .unwrap();
                }
                OllamaResponseMessage::Done => break,
            }
        }
    });
}

async fn messages_create_sse_handler(
    State(state): State<Arc<Mutex<AppState>>>,
) -> axum::response::Result<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let state = state.lock().await;

    let ollama_rx = state.ollama_rx.resubscribe();

    let sse_stream = tokio_stream::wrappers::BroadcastStream::new(ollama_rx)
        .map(|chat_chunk| {
            let chat_chunk = chat_chunk.unwrap();
            match chat_chunk {
                OllamaResponseMessage::More { mut response } => {
                    response.push(FILLED_BLOCK);
                    Event::default().event("ChatData").data(response)
                }
                OllamaResponseMessage::Done => {
                    debug!("Sending 'Done' SSE message");
                    Event::default().event("ChatDone").data("")
                }
            }
        })
        .map(Ok);

    Ok(Sse::new(sse_stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(1))
            .text("keep-alive-text"),
    ))
}

async fn conversations_create(
    State(state): State<Arc<Mutex<AppState>>>,
) -> axum::response::Result<HeaderMap> {
    let state = state.lock().await;
    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    let conversation_id: (i64,) = sqlx::query_as(
        "insert into conversations (name) values ('a new conversation') returning id;",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let conversation_id = conversation_id.0;

    let path = format!("/conversations/{conversation_id}");

    let mut headers = HeaderMap::new();
    headers.insert(
        "HX-Redirect",
        HeaderValue::try_from(path).map_err(|e| e.to_string())?,
    );

    Ok(headers)
}

async fn conversations_fork_create(
    State(state): State<Arc<Mutex<AppState>>>,
    Path((conversation_id, message_id)): Path<(i64, i64)>,
) -> axum::response::Result<HeaderMap> {
    let state = state.lock().await;
    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;
    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;

    let (new_conversation_id,): (i64,) = sqlx::query_as(
        "
        insert into conversations (name, source_conversation_id)
        values ('a new conversation', ?)
        returning id;",
    )
    .bind(conversation_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let latest_message: Message = sqlx::query_as(
        "
    select
        id,
        body,
        who,
        conversation_id,
        inserted_at
    from messages
    where id = ?
    limit 1;
    ",
    )
    .bind(message_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "
    insert into messages (who, body, conversation_id)
    select
        who,
        body,
        ?
    from messages
    where conversation_id = ?
    and messages.inserted_at <= ?
    and messages.id <= ?
    ",
    )
    .bind(new_conversation_id)
    .bind(latest_message.conversation_id)
    .bind(latest_message.inserted_at)
    .bind(message_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    let path = format!("/conversations/{new_conversation_id}");

    let mut headers = HeaderMap::new();
    headers.insert(
        "HX-Redirect",
        HeaderValue::try_from(path).map_err(|e| e.to_string())?,
    );

    Ok(headers)
}

#[derive(Deserialize)]
struct ConversationNameChangeForm {
    conversation_name: String,
}

async fn conversations_edit_get(
    Path(conversation_id): Path<i64>,
) -> axum::response::Result<Markup> {
    Ok(html! {
        div
            id="conversation-name-edit"
            class="level-item"
        {
            form
                hx-put=(format!("/conversations/{conversation_id}/edit"))
                hx-target="#conversation-name-block"
                hx-swap="outerHTML"
            {
                div class="field is-horizontal" {
                    div class="field-body" {
                        div class="field" {
                            div class="control" {
                                input type="text" name="conversation_name" class="input" placeholder="Conversation name" required;
                            }
                        }
                        div class="field" {
                            p class="control" {
                                button class="button is-link" {
                                    "Submit"
                                }
                            }
                        }
                        div class="field" {
                            p class="control" {
                                button
                                    hx-get=(format!("/conversations/{}/edit/cancel", conversation_id))
                                    hx-target="#conversation-name-edit"
                                    hx-swap="outerHTML"
                                    class="button is-link"
                                {
                                    "Cancel"
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

async fn conversations_edit_save(
    State(state): State<Arc<Mutex<AppState>>>,
    Path(conversation_id): Path<i64>,
    Form(name_change_form): Form<ConversationNameChangeForm>,
) -> axum::response::Result<Markup> {
    let state = state.lock().await;
    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    sqlx::query(
        "
    update conversations
    set name = ?
    where id = ?",
    )
    .bind(&name_change_form.conversation_name)
    .bind(conversation_id)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    Ok(html! {
        div
            id="conversation-name-block"
            class="level-left"
        {
            div class="level-item" {
                h1 class="title" {
                    (name_change_form.conversation_name)
                }
            }
            div class="level-item" {
                a hx-get=(format!("/conversations/{}/edit", conversation_id)) {
                    "Edit"
                }
            }
        }
    })
}

async fn conversations_edit_cancel(
    State(_state): State<Arc<Mutex<AppState>>>,
    Path(conversation_id): Path<i64>,
) -> axum::response::Result<Markup> {
    Ok(html! {
        div
            id="conversation-name-edit"
            class="level-item"
        {
            a
                hx-get=(format!("/conversations/{}/edit", conversation_id))
                hx-swap="outerHTML"
                hx-target="#conversation-name-edit"
            {
                "Edit"
            }
        }
    })
}

async fn conversations_delete(
    State(state): State<Arc<Mutex<AppState>>>,
    Path(conversation_id): Path<i64>,
) -> axum::response::Result<HeaderMap> {
    let state = state.lock().await;
    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    sqlx::query(
        "
    delete from conversations
    where id = ?;",
    )
    .bind(conversation_id)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let path = "/conversations/";

    let mut headers = HeaderMap::new();
    headers.insert(
        "HX-Redirect",
        HeaderValue::try_from(path).map_err(|e| e.to_string())?,
    );

    Ok(headers)
}

#[derive(Deserialize)]
struct ModelSelection {
    #[serde(rename(deserialize = "model-id"))]
    model_id: i64,
}

async fn select_model(
    State(state): State<Arc<Mutex<AppState>>>,
    Path(conversation_id): Path<i64>,
    Form(model_selection): Form<ModelSelection>,
) -> axum::response::Result<()> {
    let state = state.lock().await;

    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    sqlx::query(
        "
    update conversations
    set model_id = ?
    where id = ?
    ",
    )
    .bind(model_selection.model_id)
    .bind(conversation_id)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

#[derive(Debug)]
struct AppState {
    pool: sqlx::Pool<Sqlite>,
    http_client: reqwest::Client,
    ollama_tx: broadcast::Sender<OllamaResponseMessage>,
    ollama_rx: broadcast::Receiver<OllamaResponseMessage>,
}

#[derive(Clone, sqlx::Type, Deserialize, Serialize)]
enum Who {
    #[sqlx(rename = "Me")]
    Me,
    #[sqlx(rename = "LlaMA")]
    Llama,
}

impl Display for Who {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Who::Me => write!(f, "Me"),
            Who::Llama => write!(f, "LlaMA"),
        }
    }
}

#[derive(Debug, Parser)]
struct Config {
    #[arg(long, env, default_value = "conversations.db")]
    database: String,
    #[arg(long, env, default_value = "3000")]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::parse();

    let opts =
        sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", config.database))?
            .busy_timeout(std::time::Duration::from_secs(5))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .create_if_missing(true)
            .foreign_keys(true);

    let pool = sqlx::SqlitePool::connect_with(opts).await?;

    let mut connection = pool.acquire().await?;

    let mut txn = connection.begin().await?;

    sqlx::query(
        "create table if not exists models (
            id integer primary key autoincrement not null,
            name text not null,
            inserted_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW')),
            updated_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW'))
        );
        ",
    )
    .execute(&mut *txn)
    .await?;

    sqlx::query("create unique index if not exists models_name on models (name);")
        .execute(&mut *txn)
        .await?;

    sqlx::query(
        "create table if not exists conversations (
            id integer primary key autoincrement not null,
            name text not null,
            model_id integer not null default 1,
            source_conversation_id integer,
            inserted_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW')),
            updated_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW')),

            foreign key(model_id) references models(id)
        );",
    )
    .execute(&mut *txn)
    .await?;

    sqlx::query(
        "create table if not exists messages (
            id integer primary key autoincrement not null,
            body text not null,
            who text not null,
            conversation_id integer not null,
            inserted_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW')),
            updated_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW')),

            foreign key(conversation_id) references conversations(id) on delete cascade
        )",
    )
    .execute(&mut *txn)
    .await?;

    txn.commit().await?;

    let (ollama_tx, ollama_rx) = broadcast::channel(10);

    let http_client = reqwest::Client::new();

    let available_models = get_available_models(http_client.clone()).await.unwrap();

    let mut txn = connection.begin().await?;

    for model in available_models {
        sqlx::query(
            "
        insert into models
        (name) values (?)
        on conflict do nothing;
        ",
        )
        .bind(model)
        .execute(&mut *txn)
        .await?;
    }

    txn.commit().await?;

    let state = Arc::new(Mutex::new(AppState {
        pool,
        http_client,
        ollama_tx,
        ollama_rx,
    }));

    let app = Router::new()
        .route("/", get(conversations_index))
        .route("/conversations/", get(conversations_index))
        .route("/conversations/{id}", get(conversations_show))
        .route("/conversations/{id}/edit", get(conversations_edit_get))
        .route("/conversations/{id}/edit", put(conversations_edit_save))
        .route(
            "/conversations/{id}/edit/cancel",
            get(conversations_edit_cancel),
        )
        .route("/conversations/{id}/delete", delete(conversations_delete))
        .route(
            "/conversations/{conversation_id}/fork/{message_id}",
            post(conversations_fork_create),
        )
        .route("/conversations/new", post(conversations_create))
        .route("/messages/new", post(messages_create))
        .route("/messages/response/sse", get(messages_create_sse_handler))
        .route("/empty", get(|| async {}))
        .route("/models/select/{conversation_id}", post(select_model))
        .route(
            "/dev/state",
            get(|State(state): State<Arc<Mutex<AppState>>>| async move {
                let state = state.lock().await;
                format!("{:#?}", state)
            }),
        )
        .with_state(state)
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
