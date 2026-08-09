//! A minimal `async_stream!` replacement.
//!
//! Shared by the HTTP provider's SSE stream and the agent-CLI delegation stream. It exists so the
//! crate does not take a proc-macro dependency for two uses, and it lives here rather than in
//! `http.rs` because delegation is available without the `http` feature.

// A minimal `async_stream!` replacement, so the crate does not take a macro dependency for one use.
pub(crate) mod yielder {
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Yielder<T> {
        pub(crate) items: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<T>>>,
    }

    impl<T> Yielder<T> {
        pub async fn send(&mut self, item: T) {
            self.items.lock().expect("yielder poisoned").push_back(item);
        }
    }

    pub struct Collected<T> {
        pub(crate) items: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<T>>>,
        pub(crate) future: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    }

    impl<T: Unpin> Stream for Collected<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
            if let Some(item) = self.items.lock().expect("yielder poisoned").pop_front() {
                return Poll::Ready(Some(item));
            }
            let Some(future) = self.future.as_mut() else { return Poll::Ready(None) };
            match future.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.future = None;
                    Poll::Ready(self.items.lock().expect("yielder poisoned").pop_front())
                }
                Poll::Pending => match self.items.lock().expect("yielder poisoned").pop_front() {
                    Some(item) => Poll::Ready(Some(item)),
                    None => Poll::Pending,
                },
            }
        }
    }
}

pub(crate) fn async_stream<T, F, Fut>(f: F) -> yielder::Collected<T>
where
    T: Unpin + Send + 'static,
    F: FnOnce(yielder::Yielder<T>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let items = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let future = f(yielder::Yielder { items: std::sync::Arc::clone(&items) });
    yielder::Collected { items, future: Some(Box::pin(future)) }
}
