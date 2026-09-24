use tokio::sync::Mutex;

pub async fn bump(m: &Mutex<i32>) -> i32 {
    let mut guard = m.lock().await;
    *guard += 1;
    *guard
}

pub fn make() -> Mutex<i32> {
    Mutex::new(0, 1)
}
