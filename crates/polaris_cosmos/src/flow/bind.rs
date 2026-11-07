//! Parameterized systems with explicit typed access and captured immutable policy.
use polaris::system::{
    param::{SystemAccess, SystemContext, SystemParam},
    system::{BoxFuture, System, SystemError},
};
use std::marker::PhantomData;
/// Builds a system factory with captured policy and ordinary typed graph parameters.
/// The closure returns a boxed future so resource borrows remain tied to the context.
pub fn bind<P, T, F>(name: &'static str, func: F) -> impl FnOnce() -> BoundSystem<P, T, F>
where
    P: SystemParam + Send + Sync + 'static,
    for<'a> P::Item<'a>: Send,
    T: Send + Sync + 'static,
    F: for<'a> Fn(P::Item<'a>, &'a ()) -> BoxFuture<'a, Result<T, SystemError>>
        + Send
        + Sync
        + 'static,
{
    move || BoundSystem {
        name,
        func,
        marker: PhantomData,
    }
}
/// System returned by `bind`; parameter access is derived from `P`.
pub struct BoundSystem<P, T, F> {
    name: &'static str,
    func: F,
    marker: PhantomData<fn() -> (P, T)>,
}
impl<P, T, F> std::fmt::Debug for BoundSystem<P, T, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundSystem")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
impl<P, T, F> System for BoundSystem<P, T, F>
where
    P: SystemParam + Send + Sync + 'static,
    for<'a> P::Item<'a>: Send,
    T: Send + Sync + 'static,
    F: for<'a> Fn(P::Item<'a>, &'a ()) -> BoxFuture<'a, Result<T, SystemError>>
        + Send
        + Sync
        + 'static,
{
    type Output = T;
    fn run<'a>(&'a self, ctx: &'a SystemContext<'_>) -> BoxFuture<'a, Result<T, SystemError>> {
        Box::pin(async move { (self.func)(P::fetch(ctx)?, &()).await })
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn access(&self) -> SystemAccess {
        P::access()
    }
    fn is_fallible(&self) -> bool {
        true
    }
}
