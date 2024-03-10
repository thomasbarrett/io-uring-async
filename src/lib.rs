use std::future::Future;
use std::os::unix::prelude::{RawFd, AsRawFd};
use std::rc::Rc;
use std::cell::RefCell;
use io_uring::{IoUring};
use tokio::io::unix::AsyncFd;

// The IoUring Op state.
enum Lifecycle<C: cqueue::Entry> {
    // The Op has been pushed onto the submission queue, but has not yet
    // polled by the Rust async runtime. This state is somewhat confusingly named
    // in that an Op in the `submitted` state has not necessarily been
    // submitted to the io_uring with the `io_uring_submit` syscall.
    Submitted,
    // The Rust async runtime has polled the Op, but a completion
    // queue entry has not yet been received. When a completion queue entry is
    // received, the Waker can be used to trigger the Rust async runtime to poll
    // the Op.
    Waiting(std::task::Waker),
    // The Op has received a submission queue entry. The Op will
    // be Ready the next time that it is polled.
    Completed(C)
}

// An Future implementation that represents the current state of an IoUring Op.
pub struct Op<C: cqueue::Entry> {
    // Ownership over the OpInner value is moved to a new tokio
    // task when an Op is dropped.
    inner: Option<OpInner<C>>
}

impl<C: cqueue::Entry> Future for Op<C> {
    type Output = C;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        // It is safe to unwrap inner because it is only set to None after
        // the Op has been dropped.
        std::pin::Pin::new(self.inner.as_mut().unwrap()).poll(cx)
    }
}

impl<C: cqueue::Entry> Drop for Op<C> {
    fn drop(&mut self) {
        let inner = self.inner.take().unwrap();
        let guard = inner.slab.borrow();
        match &guard[inner.index] {
            Lifecycle::Completed(_) => {},
            _ => {
                drop(guard);
                tokio::task::spawn_local(async {
                    inner.await
                });
            }
        }
    }
}

pub struct OpInner<C: cqueue::Entry> {
    slab: Rc<RefCell<slab::Slab<Lifecycle<C>>>>,
    index: usize,
}

impl<C: cqueue::Entry> Future for OpInner<C> {
    type Output = C;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        let mut guard = self.slab.borrow_mut();
        let lifecycle = &mut guard[self.index];
        match lifecycle {
            Lifecycle::Submitted => {
                *lifecycle = Lifecycle::Waiting(cx.waker().clone());
                std::task::Poll::Pending
            }
            Lifecycle::Waiting(_) => {
                *lifecycle = Lifecycle::Waiting(cx.waker().clone());
                std::task::Poll::Pending
            }
            Lifecycle::Completed(cqe) => {
                std::task::Poll::Ready(cqe.clone())
            }
        }
    }
}

impl<C: cqueue::Entry> Drop for OpInner<C> {
    fn drop(&mut self) {
        let mut guard = self.slab.borrow_mut();
        let lifecycle = guard.remove(self.index);
        match lifecycle {
            Lifecycle::Completed(_) => {},
            _ => panic!("Op drop occured before completion")
        };
    }
}

pub mod squeue;
pub mod cqueue;

pub struct IoUringAsync<S: squeue::Entry = io_uring::squeue::Entry, C: cqueue::Entry = io_uring::cqueue::Entry> {
    uring: Rc<IoUring<S, C>>,
    slab: Rc<RefCell<slab::Slab<Lifecycle<C>>>>
}

impl<S: squeue::Entry, C: cqueue::Entry> AsRawFd for IoUringAsync<S, C> {
    fn as_raw_fd(&self) -> RawFd {
        self.uring.as_raw_fd()
    }
}

impl IoUringAsync<io_uring::squeue::Entry, io_uring::cqueue::Entry> {
    pub fn new(entries: u32) -> std::io::Result<Self> {
        Ok(Self {
            uring: Rc::new(io_uring::IoUring::generic_new(entries)?),
            slab: Rc::new(RefCell::new(slab::Slab::new()))
        })
    }
}

impl<S: squeue::Entry, C: cqueue::Entry> IoUringAsync<S, C> {
    
    pub async fn listen(uring: Rc<IoUringAsync<S, C>>) {
        let async_fd = AsyncFd::new(uring).unwrap();
        loop {
            let mut guard = async_fd.readable().await.unwrap();
            guard.get_inner().handle_cqe();
            guard.clear_ready();
        }
    }

    pub fn generic_new(entries: u32) -> std::io::Result<Self> {
        Ok(Self {
            uring: Rc::new(io_uring::IoUring::generic_new(entries)?),
            slab: Rc::new(RefCell::new(slab::Slab::new()))
        })
    }

    pub fn push(&self, entry: impl Into<S>) -> Op<C> {
        let mut guard = self.slab.borrow_mut();
        let index = guard.insert(Lifecycle::Submitted);
        let entry = entry.into().user_data(index.try_into().unwrap());
        while unsafe { self.uring.submission_shared().push(&entry).is_err() } {
            self.uring.submit().unwrap();
        }
        Op {
            inner: Some(OpInner {
                slab: self.slab.clone(),
                index: index,
            })
        }
    }

    pub fn handle_cqe(&self) {
        let mut guard = self.slab.borrow_mut();
        while let Some(cqe) = unsafe{ self.uring.completion_shared() }.next() {
            let index = cqe.user_data();
            let lifecycle = &mut guard[index.try_into().unwrap()];
            match lifecycle {
                Lifecycle::Submitted => {
                    *lifecycle = Lifecycle::Completed(cqe);
                }
                Lifecycle::Waiting(waker) => {
                    waker.wake_by_ref();
                    *lifecycle = Lifecycle::Completed(cqe);
                }
                Lifecycle::Completed(cqe) => {
                    println!("multishot operations not implemented: {}, {}", cqe.user_data(), cqe.result());
                }
            }
        }
    }

    /// Submit all queued submission queue events to the kernel.
    pub fn submit(&self) -> std::io::Result<usize> {
        self.uring.submit()
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use io_uring::opcode::Nop;
    use super::IoUringAsync;
    use send_wrapper::SendWrapper;

    #[test]
    fn example1() {
        let uring = Rc::new(IoUringAsync::new(8).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();  

        runtime.block_on(async move {
            tokio::task::LocalSet::new().run_until(async {
                tokio::task::spawn_local(IoUringAsync::listen(uring.clone()));

                let fut1 = uring.push(Nop::new().build());
                let fut2 = uring.push(Nop::new().build());
                
                uring.submit().unwrap();
                
                let cqe1 = fut1.await;
                let cqe2 = fut2.await;

                assert!(cqe1.result() >= 0, "nop error: {}", cqe1.result()); 
                assert!(cqe2.result() >= 0, "nop error: {}", cqe2.result()); 
            }).await; 
        });
    }

    #[test]
    fn example2() {
        let uring = IoUringAsync::new(8).unwrap();
        let uring = Rc::new(uring);

        // Create a new current_thread runtime that submits all outstanding submission queue
        // entries as soon as the executor goes idle.
        let uring_clone = SendWrapper::new(uring.clone());
        let runtime = tokio::runtime::Builder::new_current_thread().
            on_thread_park(move || { uring_clone.submit().unwrap(); }).
            enable_all().
            build().unwrap();  

        runtime.block_on(async move {
            tokio::task::LocalSet::new().run_until(async {
                tokio::task::spawn_local(IoUringAsync::listen(uring.clone()));

                let cqe = uring.push(Nop::new().build()).await;
                assert!(cqe.result() >= 0, "nop error: {}", cqe.result()); 
            }).await; 
        });
    }
}
