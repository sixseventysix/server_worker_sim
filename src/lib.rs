use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc::{self, Sender, Receiver}};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::sync::atomic::AtomicBool;

const MAX_CONCURRENT_TASKS: usize = 4;

// assumption 1: TASK_TIMEOUT is larger than how long any task would take to execute a request
// assumption 2: LISTENER_TIMEOUT > TASK_TIMEOUT
// because after getting a ReceivedRequest, the theoretical upper bound for execution time is TASK_TIMEOUT (from assumption 1)
// so listener will listen for a minimum of TASK_TIMEOUT so that we don't lose the output of that task by closing
// the listener thread too hastily
// this also ensures that TaskThreads are all dropped before the Listener Thread, and the Listener Thread is always dropped
// before WorkerThread is dropped, so dropping of all channels is graceful and we don't have any dangling variables.
const TASK_TIMEOUT: u64 = 2;
const LISTENER_TIMEOUT: u64 = 5;
const WORKER_TIMEOUT: u64 = 1;

type TaskId = usize;
type RequestId = usize;

pub struct Task {
    pub id: usize,
    pub query_map: HashMap<String, String>,
    pub update_map: HashMap<String, Box<dyn FnMut() + Send>>,
}

// to be returned when a TaskRequest is sent
// ReceivedRequest is sent whenever a Task receives a new TaskRequest
// this will later be followed by another TaskResult that shows the appropriate response for that TaskRequest
#[derive(Debug)]
pub enum TaskResult {
    QueryOk { req_id: RequestId, id: TaskId, value: String },
    QueryError { req_id: RequestId, id: TaskId, msg: String },
    UpdateOk { req_id: RequestId, id: TaskId },
    UpdateError { req_id: RequestId, id: TaskId, msg: String },
    NotFound { req_id: RequestId, id: TaskId, ctx: &'static str },
    Throttled { req_id: RequestId, id: TaskId },
    ReceivedRequest
}

// task requests
pub enum TaskRequest {
    CreateTask {
        req_id: RequestId,
        id: TaskId,
        query_map: HashMap<String, String>,
        update_map: HashMap<String, Box<dyn FnMut() + Send>>,
        result_tx: Sender<TaskResult>,
    },
    QueryTask {
        req_id: RequestId,
        id: TaskId,
        query_id: String,
        result_tx: Sender<TaskResult>,
    },
    UpdateTask {
        req_id: RequestId,
        id: TaskId,
        update_id: String,
        result_tx: Sender<TaskResult>,
    },
}

// enum with a similar structure to TaskRequest, but made especially for a specific Task.
// this is why the id: TaskId attribute is removed
#[derive(Debug)]
pub enum TaskControl {
    Query {
        req_id: usize,
        query_id: String,
        result_tx: Sender<TaskResult>,
    },
    Update {
        req_id: usize,
        update_id: String,
        result_tx: Sender<TaskResult>,
    },
}

// thread running task
pub struct TaskThread {
    pub task: Task,
    pub rx: Receiver<TaskControl>,
}

impl TaskThread {
    fn run(mut self) {
        let timeout_duration = Duration::from_secs(TASK_TIMEOUT);
        loop {
            println!("[Task {}] Waiting for control message...", self.task.id);
            match self.rx.recv_timeout(timeout_duration) {
                Ok(msg) => {
                    println!("[Task {}] Received control message: {:?}", self.task.id, msg);
                    match msg {
                        TaskControl::Query { req_id, query_id, result_tx } => {
                            match self.task.query_map.get(&query_id) {
                                Some(value) => {
                                    let _ = result_tx.send(TaskResult::QueryOk {
                                        req_id,
                                        id: self.task.id,
                                        value: value.clone(),
                                    });
                                }
                                None => {
                                    let _ = result_tx.send(TaskResult::QueryError {
                                        req_id,
                                        id: self.task.id,
                                        msg: format!("Query ID '{}' not found", query_id),
                                    });
                                }
                            }
                        }
                        TaskControl::Update { req_id, update_id, result_tx } => {
                            if let Some(update_fn) = self.task.update_map.get_mut(&update_id) {
                                update_fn();
                                let _ = result_tx.send(TaskResult::UpdateOk {
                                    req_id,
                                    id: self.task.id,
                                });
                            } else {
                                let _ = result_tx.send(TaskResult::UpdateError {
                                    req_id,
                                    id: self.task.id,
                                    msg: format!("Update ID '{}' not found", update_id),
                                });
                            }
                        }
                    }
                }
    
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    println!(
                        "[Task {}] No control message received for {:?}. Exiting due to inactivity.",
                        self.task.id, timeout_duration
                    );
                    break;
                }
    
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    println!(
                        "[Task {}] Control channel disconnected. Exiting task loop.",
                        self.task.id
                    );
                    break;
                }
            }
        }
    
        println!("[Task {}] Task loop terminated.", self.task.id);
    }
    
}

// thread that runs worker
pub struct WorkerThread {
    task_map: Arc<Mutex<HashMap<TaskId, Sender<TaskControl>>>>,
    active_tasks: Arc<AtomicUsize>,
}

impl WorkerThread {
    pub fn new() -> Self {
        Self {
            task_map: Arc::new(Mutex::new(HashMap::new())),
            active_tasks: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn run(
        &self,
        rx: Receiver<TaskRequest>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        let task_map = Arc::clone(&self.task_map);
        let active_tasks = Arc::clone(&self.active_tasks);

        while !shutdown_flag.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_secs(WORKER_TIMEOUT)) {
                Ok(msg) => match msg {
                    TaskRequest::CreateTask {
                        req_id,
                        id,
                        query_map,
                        update_map,
                        result_tx,
                    } => {
                        if active_tasks.load(Ordering::SeqCst) >= MAX_CONCURRENT_TASKS {
                            println!("[req:{req_id}] [WorkerThread] Task {id} rejected due to throttling");
                            let _ = result_tx.send(TaskResult::Throttled { req_id, id });
                            continue;
                        }

                        let (task_tx, task_rx) = std::sync::mpsc::channel();
                        let task = Task { id, query_map, update_map };

                        task_map.lock().unwrap().insert(id, task_tx.clone());
                        active_tasks.fetch_add(1, Ordering::SeqCst);

                        println!("[req:{req_id}] [WorkerThread] Initializing task thread for Task {id}");

                        let task_map_cloned = Arc::clone(&task_map);
                        let active_tasks_cloned = Arc::clone(&active_tasks);
                        let task_thread = TaskThread { task, rx: task_rx };

                        thread::spawn(move || {
                            task_thread.run();
                            task_map_cloned.lock().unwrap().remove(&id);
                            active_tasks_cloned.fetch_sub(1, Ordering::SeqCst);
                            println!("[WorkerThread] Task {id} finished and removed.");
                        });
                    }

                    TaskRequest::QueryTask { req_id, id, query_id, result_tx } => {
                        if let Some(tx) = task_map.lock().unwrap().get(&id) {
                            tx.send(TaskControl::Query { req_id, query_id, result_tx }).ok();
                        } else {
                            let _ = result_tx.send(TaskResult::NotFound {
                                req_id,
                                id,
                                ctx: "Task not found for query",
                            });
                        }
                    }

                    TaskRequest::UpdateTask { req_id, id, update_id, result_tx } => {
                        if let Some(tx) = task_map.lock().unwrap().get(&id) {
                            tx.send(TaskControl::Update { req_id, update_id, result_tx }).ok();
                        } else {
                            let _ = result_tx.send(TaskResult::NotFound {
                                req_id,
                                id,
                                ctx: "Task not found for update",
                            });
                        }
                    }
                },
                Err(_) => {
                    println!("[WorkerThread] recv_timeout hit, checking shutdown flag again...");
                    continue;
                }
            }
        }

        println!("[WorkerThread] Shutdown flag detected. Worker exiting.");
    }
}

pub struct ServerThread {
    pub worker_tx: Sender<TaskRequest>,
    pub result_tx: mpsc::Sender<TaskResult>,
    pub request_counter: AtomicUsize,
    pub task_id_counter: AtomicUsize,
    pub listener_handle: Option<JoinHandle<()>>,
}

impl ServerThread {
    pub fn new() -> Self {
        let (worker_tx, worker_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_for_listener = Arc::clone(&shutdown_flag);
        thread::spawn({
            let shutdown = Arc::clone(&shutdown_flag);
            move || {
                let worker = WorkerThread::new();
                worker.run(worker_rx, shutdown);
            }
        });

        let listener_handle = thread::spawn(move || {
            loop{
                match result_rx.recv_timeout(Duration::from_secs(LISTENER_TIMEOUT)) {
                    Ok(result) => {
                        match result {
                            TaskResult::QueryOk { req_id, id, value } => {
                                println!("[req:{req_id}] Query result for task {id}: {value}");
                            }
                            TaskResult::QueryError { req_id, id, msg } => {
                                println!("[req:{req_id}] Query failed for task {id}: {msg}");
                            }
                            TaskResult::UpdateOk { req_id, id } => {
                                println!("[req:{req_id}] Update OK for task {id}");
                            }
                            TaskResult::UpdateError { req_id, id, msg } => {
                                println!("[req:{req_id}] Update failed for task {id}: {msg}");
                            }
                            TaskResult::NotFound { req_id, id, ctx } => {
                                println!("[req:{req_id}] Task {id} not found: {ctx}");
                            }
                            TaskResult::Throttled { req_id, id } => {
                                println!("[req:{req_id}] Creation for task {id} failed: WorkerThread is throttled");
                            }
                            TaskResult::ReceivedRequest => {
                                println!("[Listener] Received new request. Resetting idle timer.");
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        println!("[Listener] No activity. Shutting down...");
                        shutdown_flag_for_listener.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        println!("[Listener] Channel disconnected. Shutting down...");
                        shutdown_flag_for_listener.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        Self {
            worker_tx,
            result_tx: result_tx.clone(),
            request_counter: AtomicUsize::new(0),
            task_id_counter: AtomicUsize::new(0),
            listener_handle: Some(listener_handle)
        }
    }

    pub fn next_req_id(&self) -> RequestId {
        self.request_counter.fetch_add(1, Ordering::SeqCst)
    }

    pub fn next_task_id(&self) -> TaskId {
        self.task_id_counter.fetch_add(1, Ordering::SeqCst)
    }

    pub fn create_task(
        &self,
        query_map: HashMap<String, String>,
        update_map: HashMap<String, Box<dyn FnMut() + Send>>,
    ) -> TaskId {
        let req_id = self.next_req_id();
        let id = self.next_task_id();
        println!("[req:{req_id}] [ServerThread] Sending create task to worker for Task {id}");
        self.worker_tx
            .send(TaskRequest::CreateTask {
                req_id,
                id,
                query_map,
                update_map,
                result_tx: self.result_tx.clone(),
            })
            .unwrap();

        id
    }

    pub fn query_task(&self, id: TaskId, query_id: &str) {
        let req_id = self.next_req_id();
        self.worker_tx
            .send(TaskRequest::QueryTask {
                req_id,
                id,
                query_id: query_id.to_string(),
                result_tx: self.result_tx.clone(),
            })
            .unwrap();
    }

    pub fn update_task(&self, id: TaskId, update_id: &str) {
        let req_id = self.next_req_id();
        self.worker_tx
            .send(TaskRequest::UpdateTask {
                req_id,
                id,
                update_id: update_id.to_string(),
                result_tx: self.result_tx.clone(),
            })
            .unwrap();
    }

    pub fn join_listener(self) {
        if let Some(handle) = self.listener_handle {
            let _ = handle.join();
        }
    }
}
