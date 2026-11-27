// SPDX-License-Identifier: Apache-2.0

//! Type-erased values and boxed futures, so hosts can be trait objects.

use std::any::{Any, type_name};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed, `Send` future. Mod and handler methods return it so the traits
/// stay object safe (`Box<dyn Mod>`, `Arc<dyn CommandHandler>`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A type-erased `Arc<T>`, where `T` may be unsized (`dyn Trait`).
///
/// This is how service values and contributions cross the object-safe
/// [`ModHost`](crate::ModHost) boundary. [`ModContext`](crate::ModContext)
/// wraps and unwraps it for you, so mods normally never see it.
#[derive(Clone)]
pub struct AnyArc {
    // Holds an `Arc<T>`, so unsized `T` works and `downcast` can hand back
    // the original `Arc<T>`.
    inner: Arc<dyn Any + Send + Sync>,
    type_name: &'static str,
}

impl AnyArc {
    /// Erases `value`.
    pub fn new<T>(value: Arc<T>) -> Self
    where
        T: ?Sized + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(value),
            type_name: type_name::<T>(),
        }
    }

    /// Erases a plain value, wrapping it in an `Arc` first.
    pub fn from_value<T>(value: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self::new(Arc::new(value))
    }

    /// The `Arc<T>` back, if this holds a `T`.
    pub fn downcast<T>(&self) -> Option<Arc<T>>
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.inner.downcast_ref::<Arc<T>>().cloned()
    }

    /// Whether this holds a `T`.
    pub fn is<T>(&self) -> bool
    where
        T: ?Sized + Send + Sync + 'static,
    {
        self.inner.is::<Arc<T>>()
    }

    /// Whether `self` and `other` hold the same type.
    pub fn same_type(&self, other: &AnyArc) -> bool {
        Any::type_id(&*self.inner) == Any::type_id(&*other.inner)
    }

    /// Name of the erased type, for error messages.
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }
}

impl fmt::Debug for AnyArc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AnyArc").field(&self.type_name).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Greet: Send + Sync {
        fn greet(&self) -> String;
    }
    struct En;
    impl Greet for En {
        fn greet(&self) -> String {
            "hello".into()
        }
    }

    #[test]
    fn round_trips_unsized_values() {
        let erased = AnyArc::new::<dyn Greet>(Arc::new(En));
        assert!(erased.is::<dyn Greet>());
        assert!(!erased.is::<En>());
        assert_eq!(erased.downcast::<dyn Greet>().unwrap().greet(), "hello");
        assert!(erased.downcast::<En>().is_none());
        assert!(erased.type_name().contains("Greet"));
    }

    #[test]
    fn round_trips_sized_values() {
        let erased = AnyArc::from_value(7_u32);
        assert_eq!(*erased.downcast::<u32>().unwrap(), 7);
        assert!(erased.downcast::<u64>().is_none());
        assert!(erased.same_type(&AnyArc::from_value(8_u32)));
        assert!(!erased.same_type(&AnyArc::from_value(8_u64)));
    }
}
