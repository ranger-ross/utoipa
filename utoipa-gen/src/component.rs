use std::borrow::Cow;

use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use syn::spanned::Spanned;
use syn::{Attribute, GenericArgument, Path, PathArguments, PathSegment, Type, TypePath};

use crate::doc_comment::CommentAttributes;
use crate::schema_type::SchemaFormat;
use crate::{as_tokens_or_diagnostics, Diagnostics, OptionExt, ToTokensDiagnostics};
use crate::{schema_type::SchemaType, Deprecated};

use self::features::{
    pop_feature, Feature, FeaturesExt, IsInline, Minimum, Nullable, ToTokensExt, Validatable,
};
use self::schema::format_path_ref;
use self::serde::{RenameRule, SerdeContainer, SerdeValue};

pub mod into_params;

pub mod features;
pub mod schema;
pub mod serde;

/// Check whether either serde `container_rule` or `field_rule` has _`default`_ attribute set.
#[inline]
fn is_default(container_rules: &SerdeContainer, field_rule: &SerdeValue) -> bool {
    container_rules.default || field_rule.default
}

/// Find `#[deprecated]` attribute from given attributes. Typically derive type attributes
/// or field attributes of struct.
fn get_deprecated(attributes: &[Attribute]) -> Option<Deprecated> {
    attributes.iter().find_map(|attribute| {
        if attribute
            .path()
            .get_ident()
            .map(|ident| *ident == "deprecated")
            .unwrap_or(false)
        {
            Some(Deprecated::True)
        } else {
            None
        }
    })
}

/// Check whether field is required based on following rules.
///
/// * If field has not serde's `skip_serializing_if`
/// * Field has not `serde_with` double option
/// * Field is not default
pub fn is_required(field_rule: &SerdeValue, container_rules: &SerdeContainer) -> bool {
    !field_rule.skip_serializing_if
        && !field_rule.double_option
        && !is_default(container_rules, field_rule)
}

#[cfg_attr(feature = "debug", derive(Debug))]
enum TypeTreeValue<'t> {
    TypePath(&'t TypePath),
    Path(&'t Path),
    /// Slice and array types need to be manually defined, since they cannot be recognized from
    /// generic arguments.
    Array(Vec<TypeTreeValue<'t>>, Span),
    UnitType,
    Tuple(Vec<TypeTreeValue<'t>>, Span),
}

impl PartialEq for TypeTreeValue<'_> {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Self::Path(_) => self == other,
            Self::TypePath(_) => self == other,
            Self::Array(array, _) => matches!(other, Self::Array(other, _) if other == array),
            Self::Tuple(tuple, _) => matches!(other, Self::Tuple(other, _) if other == tuple),
            Self::UnitType => self == other,
        }
    }
}

/// [`TypeTree`] of items which represents a single parsed `type` of a
/// `Schema`, `Parameter` or `FnArg`
#[cfg_attr(feature = "debug", derive(Debug))]
#[derive(Clone)]
pub struct TypeTree<'t> {
    pub path: Option<Cow<'t, Path>>,
    pub span: Option<Span>,
    pub value_type: ValueType,
    pub generic_type: Option<GenericType>,
    pub children: Option<Vec<TypeTree<'t>>>,
}

impl<'t> TypeTree<'t> {
    pub fn from_type(ty: &'t Type) -> Result<TypeTree<'t>, Diagnostics> {
        Self::convert_types(Self::get_type_tree_values(ty)?).map(|mut type_tree| {
            type_tree
                .next()
                .expect("TypeTree from type should have one TypeTree parent")
        })
    }

    fn get_type_tree_values(ty: &'t Type) -> Result<Vec<TypeTreeValue>, Diagnostics> {
        let type_tree_values = match ty {
            Type::Path(path) => {
                vec![TypeTreeValue::TypePath(path)]
            },
            Type::Reference(reference) => Self::get_type_tree_values(reference.elem.as_ref())?,
            Type::Tuple(tuple) => {
                // Detect unit type ()
                if tuple.elems.is_empty() { return Ok(vec![TypeTreeValue::UnitType]) }
                vec![TypeTreeValue::Tuple(
                    tuple.elems.iter().map(Self::get_type_tree_values).collect::<Result<Vec<_>, Diagnostics>>()?.into_iter().flatten().collect(),
                    tuple.span()
                )]
            },
            Type::Group(group) => Self::get_type_tree_values(group.elem.as_ref())?,
            Type::Slice(slice) => vec![TypeTreeValue::Array(Self::get_type_tree_values(&slice.elem)?, slice.bracket_token.span.join())],
            Type::Array(array) => vec![TypeTreeValue::Array(Self::get_type_tree_values(&array.elem)?, array.bracket_token.span.join())],
            Type::TraitObject(trait_object) => {
                trait_object
                    .bounds
                    .iter()
                    .find_map(|bound| {
                        match &bound {
                            syn::TypeParamBound::Trait(trait_bound) => Some(&trait_bound.path),
                            syn::TypeParamBound::Lifetime(_) => None,
                            syn::TypeParamBound::Verbatim(_) => None,
                            _ => todo!("TypeTree trait object found unrecognized TypeParamBound"),
                        }
                    })
                    .map(|path| vec![TypeTreeValue::Path(path)]).unwrap_or_else(Vec::new)
            }
            unexpected => return Err(Diagnostics::with_span(unexpected.span(), "unexpected type in component part get type path, expected one of: Path, Tuple, Reference, Group, Array, Slice, TraitObject")),
        };

        Ok(type_tree_values)
    }

    fn convert_types(
        paths: Vec<TypeTreeValue<'t>>,
    ) -> Result<impl Iterator<Item = TypeTree<'t>>, Diagnostics> {
        paths
            .into_iter()
            .map(|value| {
                let path = match value {
                    TypeTreeValue::TypePath(type_path) => &type_path.path,
                    TypeTreeValue::Path(path) => path,
                    TypeTreeValue::Array(value, span) => {
                        let array: Path = Ident::new("Array", span).into();
                        return Ok(TypeTree {
                            path: Some(Cow::Owned(array)),
                            span: Some(span),
                            value_type: ValueType::Object,
                            generic_type: Some(GenericType::Vec),
                            children: Some(match Self::convert_types(value) {
                                Ok(converted_values) => converted_values.collect(),
                                Err(diagnostics) => return Err(diagnostics),
                            }),
                        });
                    }
                    TypeTreeValue::Tuple(tuple, span) => {
                        return Ok(TypeTree {
                            path: None,
                            span: Some(span),
                            children: Some(match Self::convert_types(tuple) {
                                Ok(converted_values) => converted_values.collect(),
                                Err(diagnostics) => return Err(diagnostics),
                            }),
                            generic_type: None,
                            value_type: ValueType::Tuple,
                        })
                    }
                    TypeTreeValue::UnitType => {
                        return Ok(TypeTree {
                            path: None,
                            span: None,
                            value_type: ValueType::Tuple,
                            generic_type: None,
                            children: None,
                        })
                    }
                };

                // there will always be one segment at least
                let last_segment = path
                    .segments
                    .last()
                    .expect("at least one segment within path in TypeTree::convert_types");

                if last_segment.arguments.is_empty() {
                    Ok(Self::convert(path, last_segment))
                } else {
                    Self::resolve_schema_type(path, last_segment)
                }
            })
            .collect::<Result<Vec<TypeTree<'t>>, Diagnostics>>()
            .map(IntoIterator::into_iter)
    }

    // Only when type is a generic type we get to this function.
    fn resolve_schema_type(
        path: &'t Path,
        last_segment: &'t PathSegment,
    ) -> Result<TypeTree<'t>, Diagnostics> {
        if last_segment.arguments.is_empty() {
            return Err(Diagnostics::with_span(
                last_segment.ident.span(),
                "expected at least one angle bracket argument but was 0",
            ));
        };

        let mut generic_schema_type = Self::convert(path, last_segment);

        let mut generic_types = match &last_segment.arguments {
            PathArguments::AngleBracketed(angle_bracketed_args) => {
                // if all type arguments are lifetimes we ignore the generic type
                if angle_bracketed_args.args.iter().all(|arg| {
                    matches!(
                        arg,
                        GenericArgument::Lifetime(_) | GenericArgument::Const(_)
                    )
                }) {
                    None
                } else {
                    Some(
                        angle_bracketed_args
                            .args
                            .iter()
                            .filter(|arg| {
                                !matches!(
                                    arg,
                                    GenericArgument::Lifetime(_) | GenericArgument::Const(_)
                                )
                            })
                            .map(|arg| match arg {
                                GenericArgument::Type(arg) => Ok(arg),
                                unexpected => Err(Diagnostics::with_span(
                                    unexpected.span(),
                                    "expected generic argument type or generic argument lifetime",
                                )),
                            })
                            .collect::<Result<Vec<_>, Diagnostics>>()?
                            .into_iter(),
                    )
                }
            }
            _ => {
                return Err(Diagnostics::with_span(
                    last_segment.ident.span(),
                    "unexpected path argument, expected angle bracketed path argument",
                ))
            }
        };

        generic_schema_type.children = generic_types.as_mut().map_try(|generic_type| {
            generic_type
                .map(Self::from_type)
                .collect::<Result<Vec<_>, Diagnostics>>()
        })?;

        Ok(generic_schema_type)
    }

    fn convert(path: &'t Path, last_segment: &'t PathSegment) -> TypeTree<'t> {
        let generic_type = Self::get_generic_type(last_segment);
        let schema_type = SchemaType(path);

        Self {
            path: Some(Cow::Borrowed(path)),
            span: Some(path.span()),
            value_type: if schema_type.is_primitive() {
                ValueType::Primitive
            } else if schema_type.is_value() {
                ValueType::Value
            } else {
                ValueType::Object
            },
            generic_type,
            children: None,
        }
    }

    // TODO should we recognize unknown generic types with `GenericType::Unknown` instead of `None`?
    fn get_generic_type(segment: &PathSegment) -> Option<GenericType> {
        if segment.arguments.is_empty() {
            return None;
        }

        match &*segment.ident.to_string() {
            "HashMap" | "Map" | "BTreeMap" => Some(GenericType::Map),
            #[cfg(feature = "indexmap")]
            "IndexMap" => Some(GenericType::Map),
            "Vec" => Some(GenericType::Vec),
            "BTreeSet" | "HashSet" => Some(GenericType::Set),
            "LinkedList" => Some(GenericType::LinkedList),
            #[cfg(feature = "smallvec")]
            "SmallVec" => Some(GenericType::SmallVec),
            "Option" => Some(GenericType::Option),
            "Cow" => Some(GenericType::Cow),
            "Box" => Some(GenericType::Box),
            #[cfg(feature = "rc_schema")]
            "Arc" => Some(GenericType::Arc),
            #[cfg(feature = "rc_schema")]
            "Rc" => Some(GenericType::Rc),
            "RefCell" => Some(GenericType::RefCell),
            _ => None,
        }
    }

    /// Check whether [`TypeTreeValue`]'s [`syn::TypePath`] or any if it's `children`s [`syn::TypePath`]
    /// is a given type as [`str`].
    pub fn is(&self, s: &str) -> bool {
        let mut is = self
            .path
            .as_ref()
            .map(|path| {
                path.segments
                    .last()
                    .expect("expected at least one segment in TreeTypeValue path")
                    .ident
                    == s
            })
            .unwrap_or(false);

        if let Some(ref children) = self.children {
            is = is || children.iter().any(|child| child.is(s));
        }

        is
    }

    fn find_mut(&mut self, type_tree: &TypeTree) -> Option<&mut Self> {
        let is = self
            .path
            .as_mut()
            .map(|p| matches!(&type_tree.path, Some(path) if path.as_ref() == p.as_ref()))
            .unwrap_or(false);

        if is {
            Some(self)
        } else {
            self.children.as_mut().and_then(|children| {
                children
                    .iter_mut()
                    .find_map(|child| Self::find_mut(child, type_tree))
            })
        }
    }

    /// `Object` virtual type is used when generic object is required in OpenAPI spec. Typically used
    /// with `value_type` attribute to hinder the actual type.
    pub fn is_object(&self) -> bool {
        self.is("Object")
    }

    /// `Value` virtual type is used when any JSON value is required in OpenAPI spec. Typically used
    /// with `value_type` attribute for a member of type `serde_json::Value`.
    pub fn is_value(&self) -> bool {
        self.is("Value")
    }

    /// Check whether the [`TypeTree`]'s `generic_type` is [`GenericType::Option`]
    pub fn is_option(&self) -> bool {
        matches!(self.generic_type, Some(GenericType::Option))
    }

    /// Check whether the [`TypeTree`]'s `generic_type` is [`GenericType::Map`]
    pub fn is_map(&self) -> bool {
        matches!(self.generic_type, Some(GenericType::Map))
    }
}

impl PartialEq for TypeTree<'_> {
    #[cfg(feature = "debug")]
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.value_type == other.value_type
            && self.generic_type == other.generic_type
            && self.children == other.children
    }

    #[cfg(not(feature = "debug"))]
    fn eq(&self, other: &Self) -> bool {
        let path_eg = match (self.path.as_ref(), other.path.as_ref()) {
            (Some(Cow::Borrowed(self_path)), Some(Cow::Borrowed(other_path))) => {
                self_path.into_token_stream().to_string()
                    == other_path.into_token_stream().to_string()
            }
            (Some(Cow::Owned(self_path)), Some(Cow::Owned(other_path))) => {
                self_path.to_token_stream().to_string()
                    == other_path.into_token_stream().to_string()
            }
            (None, None) => true,
            _ => false,
        };

        path_eg
            && self.value_type == other.value_type
            && self.generic_type == other.generic_type
            && self.children == other.children
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Primitive,
    Object,
    Tuple,
    Value,
}

#[cfg_attr(feature = "debug", derive(Debug))]
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum GenericType {
    Vec,
    LinkedList,
    Set,
    #[cfg(feature = "smallvec")]
    SmallVec,
    Map,
    Option,
    Cow,
    Box,
    RefCell,
    #[cfg(feature = "rc_schema")]
    Arc,
    #[cfg(feature = "rc_schema")]
    Rc,
}

trait Rename {
    fn rename(rule: &RenameRule, value: &str) -> String;
}

/// Performs a rename for given `value` based on given rules. If no rules were
/// provided returns [`None`]
///
/// Method accepts 3 arguments.
/// * `value` to rename.
/// * `to` Optional rename to value for fields with _`rename`_ property.
/// * `container_rule` which is used to rename containers with _`rename_all`_ property.
fn rename<'r, R: Rename>(
    value: &'r str,
    to: Option<Cow<'r, str>>,
    container_rule: Option<&'r RenameRule>,
) -> Option<Cow<'r, str>> {
    let rename = to.and_then(|to| if !to.is_empty() { Some(to) } else { None });

    rename.or_else(|| {
        container_rule
            .as_ref()
            .map(|container_rule| Cow::Owned(R::rename(container_rule, value)))
    })
}

/// Can be used to perform rename on container level e.g `struct`, `enum` or `enum` `variant` level.
struct VariantRename;

impl Rename for VariantRename {
    fn rename(rule: &RenameRule, value: &str) -> String {
        rule.rename_variant(value)
    }
}

/// Can be used to perform rename on field level of a container e.g `struct`.
struct FieldRename;

impl Rename for FieldRename {
    fn rename(rule: &RenameRule, value: &str) -> String {
        rule.rename(value)
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub struct ComponentSchemaProps<'c> {
    pub type_tree: &'c TypeTree<'c>,
    pub features: Option<Vec<Feature>>,
    pub(crate) description: Option<&'c CommentAttributes>,
    pub(crate) deprecated: Option<&'c Deprecated>,
    pub object_name: &'c str,
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub struct ComponentSchema {
    tokens: TokenStream,
}

impl<'c> ComponentSchema {
    pub fn new(
        ComponentSchemaProps {
            type_tree,
            features,
            description,
            deprecated,
            object_name,
        }: ComponentSchemaProps,
    ) -> Result<Self, Diagnostics> {
        let mut tokens = TokenStream::new();
        let mut features = features.unwrap_or(Vec::new());
        let deprecated_stream = ComponentSchema::get_deprecated(deprecated);
        let description_stream = ComponentSchema::get_description(description);

        match type_tree.generic_type {
            Some(GenericType::Map) => ComponentSchema::map_to_tokens(
                &mut tokens,
                features,
                type_tree,
                object_name,
                description_stream,
                deprecated_stream,
            )?,
            Some(GenericType::Vec) => ComponentSchema::vec_to_tokens(
                &mut tokens,
                features,
                type_tree,
                object_name,
                description_stream,
                deprecated_stream,
            )?,
            Some(GenericType::LinkedList) => ComponentSchema::vec_to_tokens(
                &mut tokens,
                features,
                type_tree,
                object_name,
                description_stream,
                deprecated_stream,
            )?,
            Some(GenericType::Set) => ComponentSchema::vec_to_tokens(
                &mut tokens,
                features,
                type_tree,
                object_name,
                description_stream,
                deprecated_stream,
            )?,
            #[cfg(feature = "smallvec")]
            Some(GenericType::SmallVec) => ComponentSchema::vec_to_tokens(
                &mut tokens,
                features,
                type_tree,
                object_name,
                description_stream,
                deprecated_stream,
            )?,
            Some(GenericType::Option) => {
                // Add nullable feature if not already exists. Option is always nullable
                if !features
                    .iter()
                    .any(|feature| matches!(feature, Feature::Nullable(_)))
                {
                    features.push(Nullable::new().into());
                }

                ComponentSchema::new(ComponentSchemaProps {
                    type_tree: type_tree
                        .children
                        .as_ref()
                        .expect("ComponentSchema generic container type should have children")
                        .iter()
                        .next()
                        .expect("ComponentSchema generic container type should have 1 child"),
                    features: Some(features),
                    description,
                    deprecated,
                    object_name,
                })?
                .to_tokens(&mut tokens)?;
            }
            Some(GenericType::Cow) | Some(GenericType::Box) | Some(GenericType::RefCell) => {
                ComponentSchema::new(ComponentSchemaProps {
                    type_tree: type_tree
                        .children
                        .as_ref()
                        .expect("ComponentSchema generic container type should have children")
                        .iter()
                        .next()
                        .expect("ComponentSchema generic container type should have 1 child"),
                    features: Some(features),
                    description,
                    deprecated,
                    object_name,
                })?
                .to_tokens(&mut tokens)?;
            }
            #[cfg(feature = "rc_schema")]
            Some(GenericType::Arc) | Some(GenericType::Rc) => {
                ComponentSchema::new(ComponentSchemaProps {
                    type_tree: type_tree
                        .children
                        .as_ref()
                        .expect("ComponentSchema rc generic container type should have children")
                        .iter()
                        .next()
                        .expect("ComponentSchema rc generic container type should have 1 child"),
                    features: Some(features),
                    description,
                    deprecated,
                    object_name,
                })?
                .to_tokens(&mut tokens)?;
            }
            None => ComponentSchema::non_generic_to_tokens(
                &mut tokens,
                features,
                type_tree,
                object_name,
                description_stream,
                deprecated_stream,
            )?,
        };

        Ok(Self { tokens })
    }

    fn map_to_tokens(
        tokens: &mut TokenStream,
        mut features: Vec<Feature>,
        type_tree: &TypeTree,
        object_name: &str,
        description_stream: Option<TokenStream>,
        deprecated_stream: Option<TokenStream>,
    ) -> Result<(), Diagnostics> {
        let example = features.pop_by(|feature| matches!(feature, Feature::Example(_)));
        let additional_properties = pop_feature!(features => Feature::AdditionalProperties(_));
        let nullable = pop_feature!(features => Feature::Nullable(_));
        let default = pop_feature!(features => Feature::Default(_));
        let default_tokens = as_tokens_or_diagnostics!(&default);

        let additional_properties = additional_properties
            .as_ref()
            .map_try(|feature| Ok(as_tokens_or_diagnostics!(feature)))?
            .or_else_try(|| {
                // Maps are treated as generic objects with no named properties and
                // additionalProperties denoting the type
                // maps have 2 child schemas and we are interested the second one of them
                // which is used to determine the additional properties
                let schema_property = ComponentSchema::new(ComponentSchemaProps {
                    type_tree: type_tree
                        .children
                        .as_ref()
                        .expect("ComponentSchema Map type should have children")
                        .iter()
                        .nth(1)
                        .expect("ComponentSchema Map type should have 2 child"),
                    features: Some(features),
                    description: None,
                    deprecated: None,
                    object_name,
                })?;
                let schema_tokens = as_tokens_or_diagnostics!(&schema_property);

                Ok(Some(
                    quote! { .additional_properties(Some(#schema_tokens)) },
                ))
            })?;

        tokens.extend(quote! {
            utoipa::openapi::ObjectBuilder::new()
                #additional_properties
                #description_stream
                #deprecated_stream
                #default_tokens
        });

        example.to_tokens(tokens)?;
        nullable.to_tokens(tokens)
    }

    fn vec_to_tokens(
        tokens: &mut TokenStream,
        mut features: Vec<Feature>,
        type_tree: &TypeTree,
        object_name: &str,
        description_stream: Option<TokenStream>,
        deprecated_stream: Option<TokenStream>,
    ) -> Result<(), Diagnostics> {
        let example = pop_feature!(features => Feature::Example(_));
        let xml = features.extract_vec_xml_feature(type_tree)?;
        let max_items = pop_feature!(features => Feature::MaxItems(_));
        let min_items = pop_feature!(features => Feature::MinItems(_));
        let nullable = pop_feature!(features => Feature::Nullable(_));
        let default = pop_feature!(features => Feature::Default(_));

        let child = type_tree
            .children
            .as_ref()
            .expect("ComponentSchema Vec should have children")
            .iter()
            .next()
            .expect("ComponentSchema Vec should have 1 child");

        #[cfg(feature = "smallvec")]
        let child = if type_tree.generic_type == Some(GenericType::SmallVec) {
            child
                .children
                .as_ref()
                .expect("SmallVec should have children")
                .iter()
                .next()
                .expect("SmallVec should have 1 child")
        } else {
            child
        };

        let unique = matches!(type_tree.generic_type, Some(GenericType::Set));

        // is octet-stream
        let schema = if child
            .path
            .as_ref()
            .map(|path| SchemaType(path).is_byte())
            .unwrap_or(false)
        {
            quote! {
                utoipa::openapi::ObjectBuilder::new()
                    .schema_type(utoipa::openapi::schema::SchemaType::String)
                    .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(utoipa::openapi::KnownFormat::Binary)))
            }
        } else {
            let component_schema = ComponentSchema::new(ComponentSchemaProps {
                type_tree: child,
                features: Some(features),
                description: None,
                deprecated: None,
                object_name,
            })?;
            let component_schema_tokens = as_tokens_or_diagnostics!(&component_schema);

            let unique = match unique {
                true => quote! {
                    .unique_items(true)
                },
                false => quote! {},
            };

            quote! {
                utoipa::openapi::schema::ArrayBuilder::new()
                    .items(#component_schema_tokens)
                    #unique
            }
        };

        let validate = |feature: &Feature| {
            let type_path = &**type_tree.path.as_ref().unwrap();
            let schema_type = SchemaType(type_path);
            feature.validate(&schema_type, type_tree);
        };

        tokens.extend(quote! {
            #schema
            #deprecated_stream
            #description_stream
        });

        if let Some(max_items) = max_items {
            validate(&max_items);
            tokens.extend(max_items.to_token_stream())
        }

        if let Some(min_items) = min_items {
            validate(&min_items);
            tokens.extend(min_items.to_token_stream())
        }

        if let Some(default) = default {
            tokens.extend(default.to_token_stream())
        }

        example.to_tokens(tokens)?;
        xml.to_tokens(tokens)?;
        nullable.to_tokens(tokens)?;

        Ok(())
    }

    fn non_generic_to_tokens(
        tokens: &mut TokenStream,
        mut features: Vec<Feature>,
        type_tree: &TypeTree,
        object_name: &str,
        description_stream: Option<TokenStream>,
        deprecated_stream: Option<TokenStream>,
    ) -> Result<(), Diagnostics> {
        let nullable = pop_feature!(features => Feature::Nullable(_));
        let nullable_tokens = as_tokens_or_diagnostics!(&nullable);

        match type_tree.value_type {
            ValueType::Primitive => {
                let type_path = &**type_tree.path.as_ref().unwrap();
                let schema_type = SchemaType(type_path);
                if schema_type.is_unsigned_integer() {
                    // add default minimum feature only when there is no explicit minimum
                    // provided
                    if !features
                        .iter()
                        .any(|feature| matches!(&feature, Feature::Minimum(_)))
                    {
                        features.push(Minimum::new(0f64, type_path.span()).into());
                    }
                }

                let schema_type_tokens = as_tokens_or_diagnostics!(&schema_type);
                tokens.extend(quote! {
                    utoipa::openapi::ObjectBuilder::new().schema_type(#schema_type_tokens)
                });

                let format: SchemaFormat = (type_path).into();
                if format.is_known_format() {
                    tokens.extend(quote! {
                        .format(Some(#format))
                    })
                }

                tokens.extend(description_stream);
                tokens.extend(deprecated_stream);
                for feature in features.iter().filter(|feature| feature.is_validatable()) {
                    feature.validate(&schema_type, type_tree);
                }
                tokens.extend(features.to_token_stream()?);
                nullable.to_tokens(tokens)?;
            }
            ValueType::Value => {
                if type_tree.is_value() {
                    tokens.extend(quote! {
                        utoipa::openapi::ObjectBuilder::new()
                            .schema_type(utoipa::openapi::schema::SchemaType::Value)
                            #description_stream #deprecated_stream #nullable_tokens
                    })
                }
            }
            ValueType::Object => {
                let is_inline = features.is_inline();

                if type_tree.is_object() {
                    tokens.extend(quote! {
                        utoipa::openapi::ObjectBuilder::new()
                            #description_stream #deprecated_stream #nullable_tokens
                    })
                } else {
                    let type_path = &**type_tree.path.as_ref().unwrap();
                    if is_inline {
                        let default = pop_feature!(features => Feature::Default(_));
                        let default_tokens = as_tokens_or_diagnostics!(&default);
                        let schema = if default.is_some() || nullable.is_some() {
                            quote_spanned! {type_path.span()=>
                                utoipa::openapi::schema::AllOfBuilder::new()
                                    #nullable_tokens
                                    .item(<#type_path as utoipa::ToSchema>::schema().1)
                                    #default_tokens
                            }
                        } else {
                            quote_spanned! {type_path.span() =>
                                <#type_path as utoipa::ToSchema>::schema().1
                            }
                        };

                        schema.to_tokens(tokens);
                    } else {
                        let mut name = Cow::Owned(format_path_ref(type_path));
                        if name == "Self" && !object_name.is_empty() {
                            name = Cow::Borrowed(object_name);
                        }

                        let default = pop_feature!(features => Feature::Default(_));
                        let default_tokens = as_tokens_or_diagnostics!(&default);

                        let schema = if default.is_some() || nullable.is_some() {
                            quote! {
                                utoipa::openapi::schema::AllOfBuilder::new()
                                    #nullable_tokens
                                    .item(utoipa::openapi::Ref::from_schema_name(#name))
                                    #default_tokens
                            }
                        } else {
                            quote! {
                                utoipa::openapi::Ref::from_schema_name(#name)
                            }
                        };

                        schema.to_tokens(tokens);
                    }
                }
            }
            ValueType::Tuple => {
                type_tree
                    .children
                    .as_ref()
                    .map_try(|children| {
                        let all_of = children
                            .iter()
                            .map(|child| {
                                let features = if child.is_option() {
                                    Some(vec![Feature::Nullable(Nullable::new())])
                                } else {
                                    None
                                };

                                match ComponentSchema::new(ComponentSchemaProps {
                                    type_tree: child,
                                    features,
                                    description: None,
                                    deprecated: None,
                                    object_name,
                                }) {
                                    Ok(child) => Ok(as_tokens_or_diagnostics!(&child)),
                                    Err(diagnostics) => Err(diagnostics),
                                }
                            })
                            .collect::<Result<Vec<_>, Diagnostics>>()?
                            .into_iter()
                            .fold(
                                quote! { utoipa::openapi::schema::AllOfBuilder::new() },
                                |mut all_of, child_tokens| {
                                    all_of.extend(quote!( .item(#child_tokens) ));

                                    all_of
                                },
                            );

                        Result::<TokenStream, Diagnostics>::Ok(quote! {
                            utoipa::openapi::schema::ArrayBuilder::new()
                                .items(#all_of)
                            #nullable_tokens
                            #description_stream
                            #deprecated_stream
                        })
                    })?
                    .unwrap_or_else(|| quote!(utoipa::openapi::schema::empty()))
                    .to_tokens(tokens);
                tokens.extend(features.to_token_stream());
            }
        }
        Ok(())
    }

    fn get_description(comments: Option<&'c CommentAttributes>) -> Option<TokenStream> {
        comments
            .and_then(|comments| {
                let comment = CommentAttributes::as_formatted_string(comments);
                if comment.is_empty() {
                    None
                } else {
                    Some(comment)
                }
            })
            .map(|description| quote! { .description(Some(#description)) })
    }

    fn get_deprecated(deprecated: Option<&'c Deprecated>) -> Option<TokenStream> {
        deprecated.map(|deprecated| quote! { .deprecated(Some(#deprecated)) })
    }
}

impl ToTokensDiagnostics for ComponentSchema {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        self.tokens.to_tokens(tokens);
        Ok(())
    }
}

#[cfg_attr(feature = "debug", derive(Debug))]
pub struct FlattenedMapSchema {
    tokens: TokenStream,
}

impl FlattenedMapSchema {
    pub fn new(
        ComponentSchemaProps {
            type_tree,
            features,
            description,
            deprecated,
            object_name,
        }: ComponentSchemaProps,
    ) -> Result<Self, Diagnostics> {
        let mut tokens = TokenStream::new();
        let mut features = features.unwrap_or(Vec::new());
        let deprecated_stream = ComponentSchema::get_deprecated(deprecated);
        let description_stream = ComponentSchema::get_description(description);

        let example = features.pop_by(|feature| matches!(feature, Feature::Example(_)));
        let nullable = pop_feature!(features => Feature::Nullable(_));
        let default = pop_feature!(features => Feature::Default(_));
        let default_tokens = as_tokens_or_diagnostics!(&default);

        // Maps are treated as generic objects with no named properties and
        // additionalProperties denoting the type
        // maps have 2 child schemas and we are interested the second one of them
        // which is used to determine the additional properties
        let schema_property = ComponentSchema::new(ComponentSchemaProps {
            type_tree: type_tree
                .children
                .as_ref()
                .expect("ComponentSchema Map type should have children")
                .iter()
                .nth(1)
                .expect("ComponentSchema Map type should have 2 child"),
            features: Some(features),
            description: None,
            deprecated: None,
            object_name,
        })?;
        let schema_tokens = as_tokens_or_diagnostics!(&schema_property);

        tokens.extend(quote! {
            #schema_tokens
                #description_stream
                #deprecated_stream
                #default_tokens
        });

        example.to_tokens(&mut tokens)?;
        nullable.to_tokens(&mut tokens)?;

        Ok(Self { tokens })
    }
}

impl ToTokensDiagnostics for FlattenedMapSchema {
    fn to_tokens(&self, tokens: &mut TokenStream) -> Result<(), Diagnostics> {
        self.tokens.to_tokens(tokens);
        Ok(())
    }
}
