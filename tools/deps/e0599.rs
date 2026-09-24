use tokio::sync::Mutex;

pub async fn bump(m: &Mutex<i32>) -> i32 {
    let mut guard = m.lock_all().await;
    *guard += 1;
    *guard
}

pub fn make() -> Mutex<i32> {
    Mutex::new(0)
}
