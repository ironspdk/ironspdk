pub trait CEnum: Sized + Copy {
    type Repr: Copy;

    fn try_from_c(value: Self::Repr) -> Result<Self, Self::Repr>;
    fn into_c(self) -> Self::Repr;
}

/// Codegen translation between Rust and C enum constants
#[macro_export]
macro_rules! c_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident : $repr:ty {
            $(
                $variant:ident = $value:expr
            ),+ $(,)?
        }
    ) => {

        $(#[$meta])*
        #[repr($repr)]
        #[derive(Debug, Copy, Clone, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
        $vis enum $name {
            $(
                $variant = $value
            ),+
        }

        impl CEnum for $name {
            type Repr = $repr;

            #[inline]
            fn try_from_c(value: Self::Repr) -> Result<Self, Self::Repr> {
                <$name>::try_from(value).map_err(|_| value)
            }

            #[inline]
            fn into_c(self) -> Self::Repr {
                self.into()
            }
        }
    };
}
