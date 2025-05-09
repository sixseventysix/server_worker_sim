use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc::{self, Sender, Receiver}};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::sync::atomic::AtomicBool;

pub const MAX_CONCURRENT_TASKS: usize = 4;
pub const MAX_REQ_ID: usize = 100; // maximum number of request ids that can be generated

// assumption 1: TASK_TIMEOUT is larger than how long any task would take to execute a request
// assumption 2: LISTENER_TIMEOUT > TASK_TIMEOUT
// because after getting a ReceivedRequest, the theoretical upper bound for execution time is TASK_TIMEOUT (from assumption 1)
// so listener will listen for a minimum of TASK_TIMEOUT so that we don't lose the output of that task by closing
// the listener thread too hastily
// this also ensures that TaskThreads are all dropped before the Listener Thread, and the Listener Thread is always dropped
// before WorkerThread is dropped, so dropping of all channels is graceful and we don't have any dangling variables.
pub const TASK_TIMEOUT: u64 = 2;
pub const LISTENER_TIMEOUT: u64 = 5;
pub const WORKER_TIMEOUT: u64 = 5;

type TaskId = usize;
type RequestId = usize;
type SharedResults = Arc<Mutex<Vec<Option<TaskResult>>>>;

pub struct Task {
    pub id: usize,
    pub query_map: HashMap<String, String>,
    pub update_map: HashMap<String, Box<dyn FnMut() -> String + Send + 'static>>
}

// to be returned when a TaskRequest is sent
// ReceivedRequest is sent whenever a Task receives a new TaskRequest
// this will later be followed by another TaskResult that shows the appropriate response for that TaskRequest
#[derive(Debug, PartialEq, Clone)]
pub enum TaskResult {
    QueryOk { req_id: RequestId, id: TaskId, value: String },
    QueryError { req_id: RequestId, id: TaskId, msg: String },
    UpdateOk { req_id: RequestId, id: TaskId, value: String },
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
        update_map: HashMap<String, Box<dyn FnMut() -> String + Send + 'static>>,
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
// think of it as a subset of TaskRequest
#[derive(Debug)]
pub enum TaskInstruction {
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
    pub rx: Receiver<TaskInstruction>,
}

impl TaskThread {
    fn run(mut self) {
        let timeout_duration = Duration::from_secs(TASK_TIMEOUT);
        loop {
            println!("[Task {}] Waiting for instruction...", self.task.id);
            match self.rx.recv_timeout(timeout_duration) {
                Ok(msg) => {
                    println!("[Task {}] Received instruction: {:?}", self.task.id, msg);
                    // receives a TaskInstruction which it processes
                    match msg {
                        // gets value from a query_map for some query_id
                        TaskInstruction::Query { req_id, query_id, result_tx } => {
                            let _ = result_tx.send(TaskResult::ReceivedRequest);
                            // result_tx is shared directly to TaskThread via ServerThread so that it can transmit result
                            // messages directly back to ServerThread
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
                        // over here, this does not actually update any values
                        // for the sake of simplicity, it just runs some function without any parameters
                        // we assume that update_fn would alter some value (which we expect to be queried using QueryRequest)
                        TaskInstruction::Update { req_id, update_id, result_tx } => {
                            let _ = result_tx.send(TaskResult::ReceivedRequest);
                            if let Some(update_fn) = self.task.update_map.get_mut(&update_id) {
                                println!("[Task {}] Running update function", self.task.id);
                                let value = update_fn();
                                let _ = result_tx.send(TaskResult::UpdateOk {
                                    req_id,
                                    id: self.task.id,
                                    value,
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
                        "[Task {}] No instruction received for {:?}. Exiting due to inactivity.",
                        self.task.id, timeout_duration
                    );
                    break;
                }
    
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    println!(
                        "[Task {}] Worker-Task channel disconnected. Exiting task loop.",
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
    task_map: Arc<Mutex<HashMap<TaskId, Sender<TaskInstruction>>>>, // maps a Task to a transmitter that transmits from worker to task
    active_tasks: Arc<AtomicUsize>,                                 // number of active tasks (used for throttling)
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

        // while no shutdown noted
        while !shutdown_flag.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_secs(WORKER_TIMEOUT)) {
                Ok(msg) => match msg {
                    TaskRequest::CreateTask {
                        req_id,
                        id,
                        query_map,
                        update_map,
                        result_tx,
                    } => {
                        // if active tasks are more than MAX_CONCURRENT_TASKS, throttle the oncoming tasks
                        // these are assumed to be handled by the server (via a buffer)
                        // worker thread does not buffer oncoming tasks when it is throttled

                        // if the worker sees a lower value, Acquire ensures it also sees all 
                        // memory writes that were made by the task thread before its Release-ordered fetch_sub.
                        if active_tasks.load(Ordering::Acquire) >= MAX_CONCURRENT_TASKS {
                            println!("[req:{req_id}] [WorkerThread] Task {id} rejected due to throttling");
                            let _ = result_tx.send(TaskResult::Throttled { req_id, id });
                            continue;
                        }

                        let (task_tx, task_rx) = std::sync::mpsc::channel();
                        let task = Task { id, query_map, update_map };

                        task_map.lock().unwrap().insert(id, task_tx.clone());

                        // a task is created
                        // no other thread depends on seeing the increment instantly
                        // just bumping a counter — atomicity is enough, ordering doesn't matter here.
                        active_tasks.fetch_add(1, Ordering::Relaxed);

                        println!("[req:{req_id}] [WorkerThread] Initializing task thread for Task {id}");

                        let task_map_cloned = Arc::clone(&task_map);
                        let active_tasks_cloned = Arc::clone(&active_tasks);
                        let task_thread = TaskThread { task, rx: task_rx };

                        thread::spawn(move || {
                            task_thread.run();

                            // task is completed
                            task_map_cloned.lock().unwrap().remove(&id);
                            
                            // Ordering::Release says: "all memory writes before this (like removing from task_map) 
                            // must be visible to other threads that later do an Acquire load on this atomic."
                            active_tasks_cloned.fetch_sub(1, Ordering::Release);

                            println!("[WorkerThread] Task {id} finished and removed.");
                        });
                    }

                    TaskRequest::QueryTask { req_id, id, query_id, result_tx } => {
                        // get specific task
                        if let Some(tx) = task_map.lock().unwrap().get(&id) {
                            // send subset of the TaskRequest onto the specified task
                            tx.send(TaskInstruction::Query { req_id, query_id, result_tx }).ok();
                        } else {
                            let _ = result_tx.send(TaskResult::NotFound {
                                req_id,
                                id,
                                ctx: "Task not found for query",
                            });
                        }
                    }

                    TaskRequest::UpdateTask { req_id, id, update_id, result_tx } => {
                        // get specific task

                        // this unwrap will trigger if mutex lock is poisoned.
                        // but if mutex is poisoned the task_map is lost.
                        // it will be poisoned when a task thread panics.
                        // if it panics after removal from task_map, we are good. but otherwise no.
                        // currently no code exists in TaskThread that can panic so no impl against poisoned locks has been written
                        // if it panics, its fine. the task_map was in a dangerous state anyway
                        if let Some(tx) = task_map.lock().unwrap().get(&id) {
                            // send subset of the TaskRequest onto the specified task
                            tx.send(TaskInstruction::Update { req_id, update_id, result_tx }).ok();
                        } else {
                            let _ = result_tx.send(TaskResult::NotFound {
                                req_id,
                                id,
                                ctx: "Task not found for update",
                            });
                        }
                    }
                },
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // commented this println statement out so as not to overwhlem the logs
                    // happens often as server thread will close the sender as soon as all tasks are sent
                    // there might be some time in between all tasks being sent and all tasks being completed.
                    // uncommenting this would just cause a lot of annoying log messages.

                    // println!("[WorkerThread] channel is empty and sending half is closed. Exiting.");
                    // ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                    continue;
                }
                Err(e) => {
                    println!("[WorkerThread] {e}");
                    continue;
                }
            }
        }

        println!("[WorkerThread] Shutdown flag detected. Worker exiting.");
    }
}

pub struct ServerThread {
    pub worker_tx: Sender<TaskRequest>,          // transmitter from server to worker, so it has to own it
    pub result_tx: mpsc::Sender<TaskResult>,     // owns it so it can clone the mpsc::Sender and sends it to a TaskThread

    // both are AtomicUsize to ensure any operations are atomic.
    pub request_counter: usize,
    pub task_id_counter: usize,

    pub results: Arc<Mutex<Vec<Option<TaskResult>>>>,
    pub listener_handle: Option<JoinHandle<()>>, // join handle for the listener thread
}

impl ServerThread {
    pub fn new() -> Self {
        let (worker_tx, worker_rx) = mpsc::channel(); // channel for server-worker comm
        let (result_tx, result_rx) = mpsc::channel(); // channel for task-server comm for results
        
        // shutdown behaviour is based on idle time
        // if server does not send a task in a span of LISTENER_TIMEOUT idle time, listener thread shuts down as well as the worker
        // idle time gets reset every time we have confirmation of a new TaskRequest because of the behaviour of recv_timeout
        let shutdown_flag = Arc::new(AtomicBool::new(false)); // shutdown flag to be shared between listener and worker
        let shutdown_flag_for_listener = Arc::clone(&shutdown_flag);

        // chance for index overflow here
        // we are trusting that the server knows not to overflow its own vector
        let mut results_vec = Vec::with_capacity(MAX_REQ_ID);
        results_vec.resize_with(MAX_REQ_ID, || None);
        let results: SharedResults = Arc::new(Mutex::new(results_vec));
        let results_for_listener = Arc::clone(&results);

        // worker thread
        thread::spawn({
            let shutdown = Arc::clone(&shutdown_flag);
            move || {
                let worker = WorkerThread::new();
                worker.run(worker_rx, shutdown);
            }
        });

        // listener thread
        let listener_handle = thread::spawn(move || {
            loop {
                match result_rx.recv_timeout(Duration::from_secs(LISTENER_TIMEOUT)) {
                    Ok(result) => {
                        // recieved some output from a TaskThread
                        println!("[Listener] {:?}", result);
        
                        if let Some(req_id) = match &result {
                            TaskResult::QueryOk { req_id, .. }
                            | TaskResult::QueryError { req_id, .. }
                            | TaskResult::UpdateOk { req_id, .. }
                            | TaskResult::UpdateError { req_id, .. }
                            | TaskResult::NotFound { req_id, .. }
                            | TaskResult::Throttled { req_id, .. } => Some(*req_id),
                            TaskResult::ReceivedRequest => None,
                        } {
                            let mut results = results_for_listener.lock().unwrap();
                            if results.len() <= req_id {
                                results.resize(req_id + 1, None);
                            }
                            results[req_id] = Some(result);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {             // shutdown condition: idle time has reached LISTENER_TIMEOUT
                        println!("[Listener] No activity. Shutting down...");
                        shutdown_flag_for_listener.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {       // shutdown condition: channel has already been severed
                        println!("[Listener] Channel disconnected. Shutting down...");
                        shutdown_flag_for_listener.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });

        Self {
            worker_tx,
            result_tx: result_tx.clone(),
            request_counter: 0,
            task_id_counter: 0,
            results,
            listener_handle: Some(listener_handle)
        }
    }

    // current issue with these two is that they are 64 bit unsigned integers and at some point they will overflow
    // for a large system, we will need better handling of uuids than this
    // one solution could be to maintain a pool of active tasks and TaskRequests and make sure any new generated id
    // is not already in the pool
    // not implemented here

    // unique TaskRequest identifier
    pub fn next_req_id(&mut self) -> RequestId {
        let id = self.request_counter;
        self.request_counter += 1;
        id
    }

    // unique task identifier
    pub fn next_task_id(&mut self) -> TaskId {
        let id = self.task_id_counter;
        self.task_id_counter += 1;
        id
    }

    pub fn create_task(
        &mut self,
        query_map: HashMap<String, String>,
        update_map: HashMap<String, Box<dyn FnMut() -> String + Send + 'static>>
    ) -> TaskId {
        let req_id = self.next_req_id();
        let id = self.next_task_id();
        println!("[req:{req_id}] [ServerThread] Sending create task to worker for Task {id}");
        let _ = self.worker_tx
            .send(TaskRequest::CreateTask {
                req_id,
                id,
                query_map,
                update_map,
                result_tx: self.result_tx.clone(),
            });

        id
    }

    pub fn query_task(&mut self, id: TaskId, query_id: &str) {
        let req_id = self.next_req_id();
        match self.worker_tx.send(TaskRequest::QueryTask {
            req_id,
            id,
            query_id: query_id.to_string(),
            result_tx: self.result_tx.clone(),
        }) {
            Ok(()) => {
                println!("[req:{req_id}] [ServerThread] Query task {id} sent to worker.");
            }
            Err(err) => {
                println!(
                    "[req:{req_id}] [ServerThread] Failed to send query task {id} to worker: {err:?}"
                );
            }
        }
    }

    pub fn update_task(&mut self, id: TaskId, update_id: &str) {
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

    // server thread exits early, so we let the listener handle join so it can finish executing and print its logs
    // for a system without timeouts and one with an infinitely running server thread, we can use std::thread::park
    pub fn join_listener(&mut self) {
        if let Some(handle) = self.listener_handle.take() {
            let _ = handle.join();
        }
    }
}


// this block is for testing purposes
impl ServerThread {
    pub fn expect(&self, req_id: usize, expected: &TaskResult) -> bool {
        let results = self.results.lock().unwrap();
        match &results[req_id] {
            Some(actual) if actual == expected => {
                println!("[EXPECT] req:{req_id} matched expected result.");
                true
            }
            Some(actual) => {
                println!("[EXPECT] req:{req_id} mismatch.\nExpected: {:?}\nGot: {:?}", expected, actual);
                false
            },
            None => {
                println!("[EXPECT] req:{req_id} had no result.");
                false
            }
        }
    }

    pub fn expect_none(&self, req_id: usize) -> bool {
        self.results.lock().unwrap()[req_id].is_none()
    }
    
}
