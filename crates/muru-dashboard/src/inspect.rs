//! Worker inspection views (design specification sections 10.7 and
//! 11.7): what a release's workers answered to the debug operations
//! `status`, `stacks`, `tasks` and `memory`, as the dashboard's pages
//! and `muructl debug` show them. The daemon relays each worker's answer
//! as JSON, and this module reads the JSON into rows and merges the
//! stacks, so the page and the command agree on what they show.

use std::sync::Arc;

/// What is asked of a release's workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugAsk {
    /// The application.
    pub app: String,
    /// The release.
    pub release: String,
    /// The operation: status, stacks, tasks or memory.
    pub op: String,
    /// One worker, or every worker.
    pub worker: Option<u32>,
}

/// A release's workers' answers to one operation.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DebugAnswer {
    /// The release asked.
    pub release: String,
    /// Each worker's answer.
    pub workers: Vec<WorkerAnswer>,
}

/// One worker's answer, as `muructl debug --json` prints it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct WorkerAnswer {
    /// The worker's index.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Whether it answered.
    pub ok: bool,
    /// Why not, when it did not.
    pub error: Option<String>,
    /// Its answer, when it did.
    pub result: Option<serde_json::Value>,
}

/// Asks a release's workers something, or says why it could not.
pub type DebugSource =
    Arc<dyn Fn(DebugAsk) -> crate::BoxFuture<Result<DebugAnswer, String>> + Send + Sync>;

/// A worker that did not answer.
#[derive(Debug, Clone, Default)]
pub struct WorkerFailure {
    /// The worker's index.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Why.
    pub error: String,
}

/// One worker's row of `debug status`.
#[derive(Debug, Clone, Default)]
pub struct StatusRow {
    /// The worker's index.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Its group.
    pub group: String,
    /// How long it has been up, formatted.
    pub uptime: String,
    /// Requests it has served.
    pub requests: u64,
    /// Requests it is serving now.
    pub in_flight: u64,
    /// The age of its oldest request, formatted.
    pub oldest: String,
    /// Its resident size, formatted, or a dash.
    pub rss: String,
    /// Its unique size, formatted, or a dash.
    pub uss: String,
    /// starting, ready, replaced or debugging.
    pub state: String,
    /// Why it could not be asked, when it could not.
    pub error: String,
}

/// One stack, as `debug stacks` shows it: a thread's frames, with the
/// workers whose thread showed the same.
#[derive(Debug, Clone, Default)]
pub struct StackView {
    /// The thread's name.
    pub name: String,
    /// `worker 1`, or `3 workers (0, 1, 2)`.
    pub where_: String,
    /// The workers it came from.
    pub workers: Vec<u32>,
    /// The request the thread is serving, when it is.
    pub request: Option<ServingView>,
    /// Its frames, outermost first.
    pub frames: Vec<FrameView>,
}

/// The request a thread is serving.
#[derive(Debug, Clone, Default)]
pub struct ServingView {
    /// The method.
    pub method: String,
    /// The path.
    pub path: String,
    /// How long it has run, as `4.2 s`.
    pub elapsed: String,
    /// The request id.
    pub request_id: String,
}

/// One frame of a stack.
#[derive(Debug, Clone, Default)]
pub struct FrameView {
    /// The file.
    pub file: String,
    /// The line.
    pub line: u64,
    /// The function.
    pub function: String,
    /// The source line, trimmed.
    pub text: String,
}

/// One worker's answer to `debug tasks`.
#[derive(Debug, Clone, Default)]
pub struct TasksView {
    /// The worker's index.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// The event loop's lag, formatted, when it has one.
    pub lag: Option<String>,
    /// Why there are no tasks, when there is no loop.
    pub note: String,
    /// The tasks.
    pub tasks: Vec<TaskView>,
    /// Why the worker could not answer, when it could not.
    pub error: String,
}

/// One asyncio task.
#[derive(Debug, Clone, Default)]
pub struct TaskView {
    /// The task's name.
    pub name: String,
    /// The coroutine it runs.
    pub coro: String,
    /// What it awaits, or empty.
    pub waiting_on: String,
    /// Where it is, as `file:line`, or empty.
    pub at: String,
}

/// One worker's answer to `debug memory`.
#[derive(Debug, Clone, Default)]
pub struct MemoryView {
    /// The worker's index.
    pub index: u32,
    /// Its pid.
    pub pid: u32,
    /// Resident size, formatted.
    pub rss: String,
    /// Proportional set size, formatted.
    pub pss: String,
    /// Unique set size, formatted.
    pub uss: String,
    /// The garbage collector's counts by generation.
    pub gc_counts: String,
    /// The collections the runtime timed, by generation (section 11.1).
    pub gc_pauses: Vec<GcRow>,
    /// The commonest object types with their counts.
    pub objects: Vec<ObjectRow>,
    /// Why the worker could not answer, when it could not.
    pub error: String,
}

/// One generation's garbage collections since the worker started.
#[derive(Debug, Clone, Default)]
pub struct GcRow {
    /// The generation: 0, 1, 2 or full.
    pub generation: String,
    /// Collections run.
    pub collections: u64,
    /// Time spent collecting, formatted.
    pub total: String,
    /// Objects freed.
    pub collected: u64,
    /// Collections that ran while requests were in flight.
    pub busy: u64,
    /// The requests in flight during them, summed.
    pub waited: u64,
}

/// One object type and how many there are.
#[derive(Debug, Clone, Default)]
pub struct ObjectRow {
    /// The type's name.
    pub name: String,
    /// How many.
    pub count: u64,
}

/// The status rows, one per worker.
pub fn status_rows(answer: &DebugAnswer) -> Vec<StatusRow> {
    answer
        .workers
        .iter()
        .map(|worker| {
            let Some(result) = &worker.result else {
                return StatusRow {
                    index: worker.index,
                    pid: worker.pid,
                    error: worker.error.clone().unwrap_or_default(),
                    ..StatusRow::default()
                };
            };
            let size = |key: &str| {
                result[key]
                    .as_u64()
                    .map(muru_config::format_size_approx)
                    .unwrap_or_else(|| "-".to_owned())
            };

            let state = if result["replaced"].as_bool() == Some(true) {
                "replaced"
            } else if result["debugging"].as_bool() == Some(true) {
                "debugging"
            } else if result["ready"].as_bool() == Some(true) {
                "ready"
            } else {
                "starting"
            };

            StatusRow {
                index: worker.index,
                pid: worker.pid,
                group: result["group"].as_str().unwrap_or("default").to_owned(),
                uptime: format!("{:.0}s", result["uptime_s"].as_f64().unwrap_or(0.0)),
                requests: result["requests"].as_u64().unwrap_or(0),
                in_flight: result["in_flight"].as_u64().unwrap_or(0),
                oldest: format!("{}ms", result["oldest_ms"].as_u64().unwrap_or(0)),
                rss: size("rss"),
                uss: size("uss"),
                state: state.to_owned(),
                error: String::new(),
            }
        })
        .collect()
}

/// The workers that did not answer.
pub fn failures(answer: &DebugAnswer) -> Vec<WorkerFailure> {
    answer
        .workers
        .iter()
        .filter(|worker| worker.result.is_none())
        .map(|worker| WorkerFailure {
            index: worker.index,
            pid: worker.pid,
            error: worker.error.clone().unwrap_or_default(),
        })
        .collect()
}

/// The stacks merged across workers: threads with the same name and
/// frames, neither serving a request, are shown once with the workers
/// they came from, so sixteen workers waiting on the same lock read as
/// one stack.
pub fn merge_stacks(answer: &DebugAnswer) -> Vec<StackView> {
    let mut merged: Vec<StackView> = Vec::new();

    for worker in &answer.workers {
        let Some(result) = &worker.result else {
            continue;
        };

        for thread in result["threads"].as_array().into_iter().flatten() {
            let name = thread["name"].as_str().unwrap_or("?").to_owned();
            let frames: Vec<FrameView> = thread["frames"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|frame| FrameView {
                    file: frame["file"].as_str().unwrap_or("?").to_owned(),
                    line: frame["line"].as_u64().unwrap_or(0),
                    function: frame["function"].as_str().unwrap_or("?").to_owned(),
                    text: frame["text"].as_str().unwrap_or("").trim().to_owned(),
                })
                .collect();
            let request = thread["request"].as_object().map(|request| ServingView {
                method: request
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                path: request
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                elapsed: format!(
                    "{:.1} s",
                    request
                        .get("elapsed_ms")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0)
                        / 1000.0
                ),
                request_id: request
                    .get("request_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
            });
            let same = merged.iter_mut().find(|other| {
                other.name == name
                    && other.request.is_none()
                    && request.is_none()
                    && same_frames(&other.frames, &frames)
            });

            match same {
                Some(entry) => entry.workers.push(worker.index),
                None => merged.push(StackView {
                    name,
                    where_: String::new(),
                    workers: vec![worker.index],
                    request,
                    frames,
                }),
            }
        }
    }

    for stack in &mut merged {
        stack.where_ = if stack.workers.len() == 1 {
            format!("worker {}", stack.workers[0])
        } else {
            format!(
                "{} workers ({})",
                stack.workers.len(),
                stack
                    .workers
                    .iter()
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
    }

    merged
}

fn same_frames(a: &[FrameView], b: &[FrameView]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.file == b.file && a.line == b.line && a.function == b.function && a.text == b.text
        })
}

/// Each worker's tasks.
pub fn tasks_views(answer: &DebugAnswer) -> Vec<TasksView> {
    answer
        .workers
        .iter()
        .map(|worker| {
            let Some(result) = &worker.result else {
                return TasksView {
                    index: worker.index,
                    pid: worker.pid,
                    error: worker.error.clone().unwrap_or_default(),
                    ..TasksView::default()
                };
            };
            let tasks = result["tasks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|task| TaskView {
                    name: task["name"].as_str().unwrap_or("?").to_owned(),
                    coro: task["coro"].as_str().unwrap_or("?").to_owned(),
                    waiting_on: task["waiting_on"].as_str().unwrap_or("").to_owned(),
                    at: task["at"]
                        .as_object()
                        .map(|at| {
                            format!(
                                "{}:{}",
                                at.get("file")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("?"),
                                at.get("line")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0)
                            )
                        })
                        .unwrap_or_default(),
                })
                .collect();

            TasksView {
                index: worker.index,
                pid: worker.pid,
                lag: result["lag_ms"].as_f64().map(|lag| format!("{lag:.1} ms")),
                note: result["note"]
                    .as_str()
                    .unwrap_or("no event loop")
                    .to_owned(),
                tasks,
                error: String::new(),
            }
        })
        .collect()
}

/// Each worker's memory.
pub fn memory_views(answer: &DebugAnswer) -> Vec<MemoryView> {
    answer
        .workers
        .iter()
        .map(|worker| {
            let Some(result) = &worker.result else {
                return MemoryView {
                    index: worker.index,
                    pid: worker.pid,
                    error: worker.error.clone().unwrap_or_default(),
                    ..MemoryView::default()
                };
            };
            let size = |name: &str| {
                result["memory"][name]
                    .as_u64()
                    .map(muru_config::format_size_approx)
                    .unwrap_or_else(|| "?".to_owned())
            };
            let objects = result["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|entry| ObjectRow {
                    name: entry[0].as_str().unwrap_or("?").to_owned(),
                    count: entry[1].as_u64().unwrap_or(0),
                })
                .collect();

            MemoryView {
                index: worker.index,
                pid: worker.pid,
                rss: size("rss"),
                pss: size("pss"),
                uss: size("uss"),
                gc_counts: result["gc"]["counts"].to_string(),
                gc_pauses: result["gc"]["pauses"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|generation| {
                        let count = |key: &str| generation[key].as_u64().unwrap_or(0);

                        GcRow {
                            generation: generation["generation"].as_str().unwrap_or("?").to_owned(),
                            collections: count("collections"),
                            total: format!("{:.1} ms", count("ns") as f64 / 1e6),
                            collected: count("collected"),
                            busy: count("busy"),
                            waited: count("waited"),
                        }
                    })
                    .collect(),
                objects,
                error: String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(index: u32, result: serde_json::Value) -> WorkerAnswer {
        WorkerAnswer {
            index,
            pid: 100 + index,
            ok: true,
            error: None,
            result: Some(result),
        }
    }

    fn thread(name: &str, line: u64, request: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "frames": [{"file": "app.py", "line": line, "function": "view", "text": "  x = 1 "}],
            "request": request,
        })
    }

    #[test]
    fn stacks_merge_across_workers_unless_a_thread_serves_a_request() {
        let serving = serde_json::json!({
            "method": "GET", "path": "/slow", "elapsed_ms": 4200.0, "request_id": "01ABC"
        });
        let answer = DebugAnswer {
            release: "r1".to_owned(),
            workers: vec![
                answered(
                    0,
                    serde_json::json!({"threads": [thread("MainThread", 3, serde_json::Value::Null)]}),
                ),
                answered(
                    1,
                    serde_json::json!({"threads": [
                        thread("MainThread", 3, serde_json::Value::Null),
                        thread("muru-app-0", 9, serving),
                    ]}),
                ),
                WorkerAnswer {
                    index: 2,
                    pid: 102,
                    ok: false,
                    error: Some("did not answer".to_owned()),
                    result: None,
                },
            ],
        };

        let stacks = merge_stacks(&answer);
        assert_eq!(stacks.len(), 2);
        assert_eq!(stacks[0].name, "MainThread");
        assert_eq!(stacks[0].where_, "2 workers (0, 1)");
        assert_eq!(stacks[0].frames[0].text, "x = 1");
        assert_eq!(stacks[1].where_, "worker 1");
        let request = stacks[1].request.as_ref().expect("serving");
        assert_eq!(request.elapsed, "4.2 s");
        assert_eq!(request.path, "/slow");

        let failed = failures(&answer);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].index, 2);
        assert_eq!(failed[0].error, "did not answer");
    }

    #[test]
    fn status_tasks_and_memory_read_the_workers_answers() {
        let answer = DebugAnswer {
            release: "r1".to_owned(),
            workers: vec![answered(
                0,
                serde_json::json!({
                    "group": "hot", "ready": true, "replaced": false, "debugging": false,
                    "uptime_s": 12.4, "requests": 7, "in_flight": 1, "oldest_ms": 30,
                    "rss": 50_000_000u64, "uss": 30_000_000u64,
                    "lag_ms": 0.4,
                    "tasks": [{"name": "Task-1", "coro": "serve", "waiting_on": "<Future>",
                               "at": {"file": "app.py", "line": 4}}],
                    "memory": {"rss": 50_000_000u64, "pss": 40_000_000u64, "uss": 30_000_000u64},
                    "gc": {"counts": [1, 2, 3]},
                    "objects": [["dict", 1200], ["str", 900]],
                }),
            )],
        };

        let status = status_rows(&answer);
        assert_eq!(status[0].group, "hot");
        assert_eq!(status[0].state, "ready");
        assert_eq!(status[0].uptime, "12s");
        assert_eq!(status[0].rss, "48MB");

        let tasks = tasks_views(&answer);
        assert_eq!(tasks[0].lag.as_deref(), Some("0.4 ms"));
        assert_eq!(tasks[0].tasks[0].at, "app.py:4");
        assert_eq!(tasks[0].tasks[0].waiting_on, "<Future>");

        let memory = memory_views(&answer);
        assert_eq!(memory[0].pss, "38MB");
        assert_eq!(memory[0].gc_counts, "[1,2,3]");
        assert_eq!(memory[0].objects[0].name, "dict");
        assert_eq!(memory[0].objects[0].count, 1200);
    }
}
