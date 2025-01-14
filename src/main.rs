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
// - [ ] back button on `show`
// - [x] edit conversation names
// - [ ] delete messages
// - [x] delete conversations (show)
// - [ ] bulk delete conversations (index)
// - [ ] cmd+enter to send messages
// - [ ] fix Option::take panic

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::sse::Event;
use axum::response::Sse;
use axum::routing::{delete, get, post, put};
use axum::{Form, Router};
use clap::Parser;
use futures::Stream;
use maud::{html, Markup, DOCTYPE};
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, Sqlite};
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

#[derive(Deserialize, Clone, Debug)]
struct ChatChunk {
    // model: String,
    // created_at: String,
    response: String,
    done: bool,
}

async fn send_chat_message(
    client: reqwest::Client,
    message: &Message,
    history: &mut Vec<Message>,
) -> anyhow::Result<(
    tokio::sync::broadcast::Receiver<ChatChunk>,
    tokio::sync::broadcast::Receiver<ChatChunk>,
)> {
    let (tx, rx1) = tokio::sync::broadcast::channel(10);

    let rx2 = tx.subscribe();

    history.push(message.clone());

    let mut prompt = String::new();

    for message in history {
        prompt.push_str(&message.who.to_string());
        prompt.push_str(": ");
        prompt.push_str(&message.body);
        prompt.push('\n');
    }

    prompt.push_str("You:");

    let body = HashMap::from([("model", "llama3.3".to_string()), ("prompt", prompt)]);

    tokio::spawn(async move {
        let mut resp = client
            .post("http://localhost:11434/api/generate")
            .json(&body)
            .send()
            .await
            .unwrap();

        while let Some(chunk) = resp.chunk().await.unwrap() {
            if let Ok(chunk) = serde_json::from_slice(&chunk) {
                let _ = tx.send(chunk);
            };
        }
    });

    Ok((rx1, rx2))
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct Message {
    id: i64,
    body: String,
    who: String,
    conversation_id: i64,
    inserted_at: String,
    updated_at: String,
}

#[derive(sqlx::FromRow)]
struct Conversation {
    id: i64,
    name: String,
    source_conversation_id: Option<i64>,
    source_conversation_name: Option<String>,
    inserted_at: String,
    updated_at: String,
}

#[derive(sqlx::FromRow)]
struct ConversationWithLastMessageTime {
    id: i64,
    name: String,
    source_conversation_id: Option<i64>,
    source_conversation_name: Option<String>,
    inserted_at: String,
    updated_at: String,
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
            conversations.updated_at,
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
            script src="https://unpkg.com/htmx.org@2.0.4" {}
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
                table class="table" {
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
    let mut state = state.lock().await;

    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

    let conversation: Conversation = sqlx::query_as(
        "
    select
        conversations.id,
        conversations.name,
        conversations.source_conversation_id,
        c2.name as source_conversation_name,
        conversations.inserted_at,
        conversations.updated_at
    from conversations
    left join conversations c2
        on conversations.source_conversation_id = c2.id
    where conversations.id = ?;
    ",
    )
    .bind(conversation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let messages: Vec<Message> = sqlx::query_as(
        "
        select
            id,
            body,
            who,
            conversation_id,
            inserted_at,
            updated_at
         from messages where conversation_id = ?;",
    )
    .bind(conversation_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    state.history = messages.clone();

    Ok(html! {
        (DOCTYPE)
        head {
            meta charset="UTF-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            title {
                "conversations"
            }
            script src="https://unpkg.com/htmx.org@2.0.4" {}
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
        }
        body {
            div class="container mb-5" {
                section class="section" {
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
                }

                table class="table" {
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
                        hx-post="/messages/new"
                        hx-target="#messages"
                        hx-swap="beforeend"
                        hx-on::after-request=" if(event.detail.successful) this.reset()"
                    {
                        div class="field" {
                            div class="control" {
                                textarea class="textarea" name="body" {}
                            }
                        }

                        input type="hidden" name="conversation_id" value=(conversation.id) {}
                        div class="field is-grouped" {
                            div class="control" {
                                button class="button is-link" {
                                    "Send"
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
    let state_for_response = Arc::clone(&state);

    let mut state = state.lock().await;

    let mut conn = state.pool.acquire().await.map_err(|e| e.to_string())?;

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
        inserted_at,
        updated_at;",
    )
    .bind(Who::Me)
    .bind(message_send_form.body)
    .bind(message_send_form.conversation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let count_row: (i64,) = sqlx::query_as(
        "select
        count(*)
    from messages
    where conversation_id = ?;
    ",
    )
    .bind(message_send_form.conversation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let count = count_row.0;

    let previous_llama_sse_response: Option<Message> = sqlx::query_as(
        "
        select
            *
        from
        messages
        where conversation_id = ?
        and who = 'LlaMA'
        order by inserted_at desc
        limit 1;",
    )
    .bind(message_send_form.conversation_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    let client = state.http_client.clone();

    let (ollama_rx1, ollama_rx2) = send_chat_message(client, &message, &mut state.history)
        .await
        .map_err(|e| e.to_string())?;

    state.ollama_rx = Some(ollama_rx1);

    let conn2 = state.pool.acquire().await.map_err(|e| e.to_string())?;

    let conversation_id = message_send_form.conversation_id;

    tokio::spawn(async move {
        let state = state_for_response;
        let conversation_id = conversation_id;
        let mut conn = conn2;
        let mut ollama_rx = ollama_rx2;

        let mut response = String::new();

        while let Ok(chat_chunk) = ollama_rx.recv().await {
            response.push_str(&chat_chunk.response)
        }

        let message: Message = sqlx::query_as(
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
        .bind(response)
        .bind(conversation_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| e.to_string())
        .unwrap();

        let mut state = state.lock().await;
        state.history.push(message);
    });

    Ok(html! {
            @if let Some(m) = previous_llama_sse_response {
                template {
                    tr hx-swap-oob="outerHTML:tr#previous-llama-sse-response" {
                        td {
                            (count - 1)
                        }
                        td {
                            (m.inserted_at)
                        }
                        td {
                            (m.who.to_string())
                        }
                        td {
                            pre {
                                (m.body)
                            }
                        }
                        td {
                            a hx-post=(format!("/conversations/{}/fork/{}", m.conversation_id, m.id)) {
                                "Fork"
                            }
                        }
                    }
                }
            }
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
            tr id="previous-llama-sse-response" {
                td {
                    (count + 1)
                }
                td {
                    ""
                }
                td {
                    (Who::Llama)
                }
                td {
                    pre
                        hx-ext="sse"
                        sse-connect="/messages/response/sse"
                        sse-swap="NotDone"
                        sse-close="Done"
                        hx-swap="beforeend"
                    {
                        ""
                    }
                }
                td {
                    // Can't fork yet...need the message id once it's actually inserted
                }
            }
    })
}

async fn messages_create_sse_handler(
    State(state): State<Arc<Mutex<AppState>>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let ollama_rx = {
        let mut state = state.lock().await;

        // TODO figure out how to swap something in here...
        let rx = state.ollama_rx.take().unwrap();

        tokio_stream::wrappers::BroadcastStream::new(rx)
            .map(|chat_chunk| {
                let cc = chat_chunk.unwrap();
                if cc.done {
                    Event::default().event("Done").data("")
                } else {
                    Event::default().event("NotDone").data(cc.response)
                }
            })
            .map(Ok)
    };

    Sse::new(ollama_rx).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(std::time::Duration::from_secs(1))
            .text("keep-alive-text"),
    )
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
        *
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

#[derive(Debug)]
struct AppState {
    pool: sqlx::Pool<Sqlite>,
    history: Vec<Message>,
    http_client: reqwest::Client,
    ollama_rx: Option<tokio::sync::broadcast::Receiver<ChatChunk>>,
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
        "create table if not exists conversations (
            id integer primary key autoincrement not null,
            name text not null,
            source_conversation_id integer,
            inserted_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW')),
            updated_at datetime not null default(STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW'))
        )",
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

    let state = Arc::new(Mutex::new(AppState {
        pool,
        history: vec![],
        http_client: reqwest::Client::new(),
        ollama_rx: None,
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
