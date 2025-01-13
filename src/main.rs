// TODO:
// - [ ] ability to "branch" from a certain portion of the conversation, by
//       creating a new conversation up to a certain point
// - [ ] auto reload in dev
// - [ ] request logging (with tower)
// - [ ] search
// - [ ] tagging

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::sse::Event;
use axum::response::Sse;
use axum::routing::{get, post};
use axum::{Form, Router};
use futures::Stream;
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use sqlx::{Acquire, Sqlite};
use std::collections::HashMap;
use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;

#[derive(Deserialize, Clone, Debug)]
struct ChatChunk {
    model: String,
    created_at: String,
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

    // let mut prompt = history
    //     .iter()
    //     .map(|message| format!("{}: {}\n", message.who, message.body))
    //     .collect::<String>();

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

#[derive(Clone, sqlx::FromRow)]
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
    inserted_at: String,
    updated_at: String,
}

#[derive(sqlx::FromRow)]
struct ConversationWithLastMessageTime {
    id: i64,
    name: String,
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
        order by inserted_at desc;
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
            h1 {
                "conversations"
            }

            button hx-post="/conversations/new" {
                "new conversation"
            }
            table class="table" {
                thead {
                    tr {
                        th { "started" }
                        th { "last message" }
                        th { "name" }
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

    let conversation: Conversation = sqlx::query_as("select * from conversations where id = ?;")
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
                        div class="level-left" {
                            div class="level-item" {
                                h1 class="title" {
                                    (conversation.name)
                                }
                            }
                            div class="level-item" {
                                a href="" {
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
                        // button type="submit" { "Send" }
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
    .bind("Me")
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
        .bind("LlaMA")
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
                    tr hx-swap-oob="outerHTML:#previous-llama-sse-response" {
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
            }
            tr id="previous-llama-sse-response" {
                td {
                    (count + 1)
                }
                td {
                    ""
                }
                td {
                    "LlaMA"
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

struct AppState {
    pool: sqlx::Pool<Sqlite>,
    history: Vec<Message>,
    http_client: reqwest::Client,
    ollama_rx: Option<tokio::sync::broadcast::Receiver<ChatChunk>>,
}

// #[derive(Clone, sqlx::Type, Deserialize, Serialize)]
// #[sqlx(type_name = "Who")]
// enum Who {
//     Me,
//     Llama,
// }

// impl Display for Who {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         match self {
//             Who::Me => write!(f, "Me"),
//             Who::Llama => write!(f, "LlaMA"),
//         }
//     }
// }

// impl FromStr for Who {
//     type Err = &'static str;

//     fn from_str(s: &str) -> Result<Self, Self::Err> {
//         match s {
//             "Me" => Ok(Who::Me),
//             "LlaMA" => Ok(Who::Llama),
//             _ => Err("must be Me or LlaMA"),
//         }
//     }
// }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite://conversations.db")?
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
            inserted_at datetime not null default current_timestamp,
            updated_at datetime not null default current_timestamp
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
            inserted_at datetime not null default current_timestamp,
            updated_at datetime not null default current_timestamp,

            foreign key(conversation_id) references conversations(id)
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
        .route("/conversations/new", post(conversations_create))
        .route("/messages/new", post(messages_create))
        .route("/messages/response/sse", get(messages_create_sse_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
