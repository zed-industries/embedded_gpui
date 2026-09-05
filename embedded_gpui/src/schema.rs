//! Interfaces and payload types as data.
//!
//! `#[interface]` implements [`Interface::schema`](crate::Interface::schema) and `#[data]`
//! implements [`Describe`], so the same schema the Rust macros compile against is
//! available at runtime: to a dynamic-language guest binding method calls, to an
//! inspector, or to a bindings generator (see [`typescript`](crate::typescript)).
//!
//! The wire format is JSON, so the type vocabulary here is JSON's plus two
//! distinguished shapes: a [`TypeSchema::Ref`] travels as `{"$ref": index}` into the
//! payload's ref table, and a named type is a `#[data]` struct or enum whose definition
//! is listed once in [`Schema::types`].

use std::collections::HashMap;

use crate::{Interface, Ref};

/// An interface described as data.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub name: &'static str,
    pub methods: Vec<MethodSchema>,
    pub events: Vec<EventSchema>,
    /// Every named type the methods and events mention, transitively, each once.
    pub types: Vec<TypeDefinition>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodSchema {
    pub name: &'static str,
    pub arguments: Vec<ArgumentSchema>,
    /// The response type. For methods declared to return `Ref<T>` this is
    /// [`TypeSchema::Ref`]; callers resolve it to a connected remote.
    pub response: TypeSchema,
    pub is_async: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArgumentSchema {
    pub name: &'static str,
    pub ty: TypeSchema,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventSchema {
    pub name: &'static str,
    pub ty: TypeSchema,
}

/// The shape of a value in a payload.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeSchema {
    /// `()`: JSON `null`.
    Unit,
    Bool,
    /// Any Rust integer; JSON has one number type.
    Integer,
    Float,
    String,
    Option(Box<TypeSchema>),
    List(Box<TypeSchema>),
    /// A string-keyed map.
    Map(Box<TypeSchema>),
    Tuple(Vec<TypeSchema>),
    /// A capability to an object of the named interface, carried in the ref table.
    Ref(&'static str),
    /// A `#[data]` struct or enum, defined in [`Schema::types`].
    Named(&'static str),
}

/// The definition behind a [`TypeSchema::Named`].
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDefinition {
    pub name: &'static str,
    pub kind: TypeKind,
}

/// serde's default (externally tagged) representations, which is what `#[data]` uses.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    /// A struct with named fields: a JSON object.
    Struct(Vec<FieldSchema>),
    /// An enum: a unit variant is its name as a string; a variant with fields is
    /// `{"Variant": ...}` with an object (named fields) or a value/array (unnamed).
    Enum(Vec<VariantSchema>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldSchema {
    pub name: &'static str,
    pub ty: TypeSchema,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VariantSchema {
    pub name: &'static str,
    pub fields: VariantFields,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VariantFields {
    Unit,
    Named(Vec<FieldSchema>),
    Unnamed(Vec<TypeSchema>),
}

/// A type that can appear in a payload and describe its own shape. Implemented for the
/// JSON-shaped primitives and containers here, for `Ref`, and for every `#[data]` type.
pub trait Describe {
    fn describe() -> TypeSchema;

    /// Add this type's named definitions (its own, if any, then its components') to
    /// `types`, each once.
    fn collect(types: &mut Vec<TypeDefinition>) {
        let _ = types;
    }
}

/// Add a definition unless one with the same name is already present.
pub fn collect_definition(types: &mut Vec<TypeDefinition>, definition: TypeDefinition) -> bool {
    if types
        .iter()
        .any(|existing| existing.name == definition.name)
    {
        return false;
    }
    types.push(definition);
    true
}

macro_rules! describe_as {
    ($schema:expr => $($ty:ty),* $(,)?) => {
        $(impl Describe for $ty {
            fn describe() -> TypeSchema {
                $schema
            }
        })*
    };
}

describe_as!(TypeSchema::Unit => ());
describe_as!(TypeSchema::Bool => bool);
describe_as!(TypeSchema::Integer => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
describe_as!(TypeSchema::Float => f32, f64);
describe_as!(TypeSchema::String => String, char);

impl<T: Describe> Describe for Option<T> {
    fn describe() -> TypeSchema {
        TypeSchema::Option(Box::new(T::describe()))
    }

    fn collect(types: &mut Vec<TypeDefinition>) {
        T::collect(types);
    }
}

impl<T: Describe> Describe for Vec<T> {
    fn describe() -> TypeSchema {
        TypeSchema::List(Box::new(T::describe()))
    }

    fn collect(types: &mut Vec<TypeDefinition>) {
        T::collect(types);
    }
}

impl<V: Describe, S> Describe for HashMap<String, V, S> {
    fn describe() -> TypeSchema {
        TypeSchema::Map(Box::new(V::describe()))
    }

    fn collect(types: &mut Vec<TypeDefinition>) {
        V::collect(types);
    }
}

impl<S: Interface> Describe for Ref<S> {
    fn describe() -> TypeSchema {
        TypeSchema::Ref(S::schema().name)
    }
}

macro_rules! describe_tuples {
    ($(($($name:ident),+))+) => {
        $(impl<$($name: Describe),+> Describe for ($($name,)+) {
            fn describe() -> TypeSchema {
                TypeSchema::Tuple(vec![$($name::describe()),+])
            }

            fn collect(types: &mut Vec<TypeDefinition>) {
                $($name::collect(types);)+
            }
        })+
    };
}

describe_tuples!((A)(A, B)(A, B, C)(A, B, C, D));
