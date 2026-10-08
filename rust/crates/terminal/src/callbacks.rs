use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
pub(crate) type Writer = Rc<dyn Fn(String)>;
#[derive(Default)]
pub(crate) struct Writers {
    next: Cell<u32>,
    callbacks: RefCell<BTreeMap<u32, Writer>>,
}
impl Writers {
    pub(crate) fn attach<E>(
        &self,
        writer: Writer,
        index: u32,
        mut set: impl FnMut(u32, u32) -> Result<(), E>,
    ) -> Result<u32, E> {
        let id = self
            .next
            .get()
            .checked_add(1)
            .expect("terminal writer ID exhausted");
        self.next.set(id);
        self.callbacks.borrow_mut().insert(id, writer);
        let result = set(0, id).and_then(|()| set(1, index));
        if let Err(cause) = result {
            let _ = set(1, 0);
            let _ = set(0, 0);
            self.remove(id);
            return Err(cause);
        }
        Ok(id)
    }
    pub(crate) fn remove(&self, id: u32) {
        self.callbacks.borrow_mut().remove(&id);
    }
    pub(crate) fn deliver(&self, id: u32, data: String) {
        // PTY responses can synchronously reenter another terminal or detach.
        let callback = self.callbacks.borrow().get(&id).cloned();
        if let Some(callback) = callback {
            callback(data)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_delivery_and_detach_release_registry_borrow() {
        let writers = Rc::new(Writers::default());
        let seen = Rc::new(RefCell::new(vec![]));
        let output = seen.clone();
        let second = writers
            .attach(Rc::new(move |s| output.borrow_mut().push(s)), 7, |_, _| {
                Ok::<_, ()>(())
            })
            .unwrap();
        let registry = writers.clone();
        let output = seen.clone();
        let first = writers
            .attach(
                Rc::new(move |s| {
                    output.borrow_mut().push(s);
                    registry.deliver(second, "second".into());
                    registry.remove(second)
                }),
                7,
                |_, _| Ok::<_, ()>(()),
            )
            .unwrap();
        writers.deliver(first, "first".into());
        writers.deliver(second, "removed".into());
        assert_eq!(*seen.borrow(), vec!["first", "second"]);
    }
    #[test]
    fn either_failed_set_rolls_back_callback_userdata_and_registry() {
        for fail in 0..2 {
            let writers = Writers::default();
            let mut calls = vec![];
            let result = writers.attach(
                Rc::new(|_| panic!("rolled back writer called")),
                7,
                |option, value| {
                    calls.push((option, value));
                    if calls.len() == fail + 1 {
                        Err("failure")
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err("failure"));
            assert_eq!(&calls[calls.len() - 2..], &[(1, 0), (0, 0)]);
            assert!(writers.callbacks.borrow().is_empty());
            writers.deliver(1, "ignored".into());
        }
    }
}
