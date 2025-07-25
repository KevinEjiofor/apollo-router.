mod fields;

use std::fmt::Debug;
use std::fmt::Formatter;
use std::iter;
use std::sync::Arc;

use apollo_compiler::Name;
use apollo_compiler::Node;
use apollo_compiler::Schema;
use apollo_compiler::ast::Argument;
use apollo_compiler::ast::Directive;
use apollo_compiler::ast::DirectiveDefinition;
use apollo_compiler::ast::DirectiveList;
use apollo_compiler::ast::DirectiveLocation;
use apollo_compiler::ast::EnumValueDefinition;
use apollo_compiler::ast::FieldDefinition;
use apollo_compiler::ast::NamedType;
use apollo_compiler::ast::Type;
use apollo_compiler::ast::Value;
use apollo_compiler::collections::HashMap;
use apollo_compiler::collections::IndexMap;
use apollo_compiler::collections::IndexSet;
use apollo_compiler::name;
use apollo_compiler::schema::Component;
use apollo_compiler::schema::EnumType;
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::schema::Implementers;
use apollo_compiler::schema::InputObjectType;
use apollo_compiler::schema::InputValueDefinition;
use apollo_compiler::schema::InterfaceType;
use apollo_compiler::schema::ObjectType;
use apollo_compiler::schema::ScalarType;
use apollo_compiler::schema::UnionType;
use apollo_compiler::ty;
use apollo_compiler::validation::Valid;
use indexmap::map::Entry::Occupied;
use indexmap::map::Entry::Vacant;
use indexmap::map::Iter;
use itertools::Itertools;

use crate::ValidFederationSubgraph;
use crate::ValidFederationSubgraphs;
use crate::error::FederationError;
use crate::link::LinksMetadata;
use crate::link::federation_spec_definition::FEDERATION_EXTERNAL_DIRECTIVE_NAME_IN_SPEC;
use crate::link::federation_spec_definition::FEDERATION_FIELDS_ARGUMENT_NAME;
use crate::link::federation_spec_definition::FEDERATION_FROM_ARGUMENT_NAME;
use crate::link::federation_spec_definition::FEDERATION_INTERFACEOBJECT_DIRECTIVE_NAME_IN_SPEC;
use crate::link::federation_spec_definition::FEDERATION_KEY_DIRECTIVE_NAME_IN_SPEC;
use crate::link::federation_spec_definition::FEDERATION_OVERRIDE_DIRECTIVE_NAME_IN_SPEC;
use crate::link::federation_spec_definition::FEDERATION_OVERRIDE_LABEL_ARGUMENT_NAME;
use crate::link::federation_spec_definition::FEDERATION_PROVIDES_DIRECTIVE_NAME_IN_SPEC;
use crate::link::federation_spec_definition::FEDERATION_REQUIRES_DIRECTIVE_NAME_IN_SPEC;
use crate::link::inaccessible_spec_definition::INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC;
use crate::link::inaccessible_spec_definition::InaccessibleSpecDefinition;
use crate::link::join_spec_definition::JOIN_OVERRIDE_LABEL_ARGUMENT_NAME;
use crate::link::spec::Identity;
use crate::link::spec::Version;
use crate::link::spec_definition::SpecDefinition;
use crate::schema::ValidFederationSchema;
use crate::subgraph::ValidSubgraph;

type MergeWarning = String;
type MergeError = String;

struct Merger {
    errors: Vec<MergeError>,
    composition_hints: Vec<MergeWarning>,
    needs_inaccessible: bool,
    interface_objects: IndexSet<Name>,
}

pub struct MergeSuccess {
    pub schema: Valid<Schema>,
    pub composition_hints: Vec<MergeWarning>,
}

impl From<FederationError> for MergeFailure {
    fn from(err: FederationError) -> Self {
        // TODO: Consider an easier transition / interop between MergeFailure and FederationError
        // TODO: This is most certainly not the right error kind. MergeFailure's
        // errors need to be in an enum that could be matched on rather than a
        // str.
        MergeFailure {
            schema: None,
            errors: vec![err.to_string()],
            composition_hints: vec![],
        }
    }
}

pub struct MergeFailure {
    pub schema: Option<Box<Schema>>,
    pub errors: Vec<MergeError>,
    pub composition_hints: Vec<MergeWarning>,
}

impl Debug for MergeFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("MergeFailure")
            .field("errors", &self.errors)
            .field("composition_hints", &self.composition_hints)
            .finish()
    }
}

pub fn merge_subgraphs(subgraphs: Vec<&ValidSubgraph>) -> Result<MergeSuccess, MergeFailure> {
    let mut merger = Merger::new();
    let mut federation_subgraphs = ValidFederationSubgraphs::new();
    for subgraph in subgraphs {
        federation_subgraphs.add(ValidFederationSubgraph {
            name: subgraph.name.clone(),
            url: subgraph.url.clone(),
            schema: ValidFederationSchema::new(subgraph.schema.clone())?,
        })?;
    }
    merger.merge(federation_subgraphs)
}

pub fn merge_federation_subgraphs(
    subgraphs: ValidFederationSubgraphs,
) -> Result<MergeSuccess, MergeFailure> {
    let mut merger = Merger::new();
    merger.merge(subgraphs)
}

impl Merger {
    fn new() -> Self {
        Merger {
            composition_hints: Vec::new(),
            errors: Vec::new(),
            needs_inaccessible: false,
            interface_objects: IndexSet::default(),
        }
    }

    fn merge(&mut self, subgraphs: ValidFederationSubgraphs) -> Result<MergeSuccess, MergeFailure> {
        let mut subgraphs = subgraphs
            .into_iter()
            .map(|(_, subgraph)| subgraph)
            .collect_vec();
        subgraphs.sort_by(|s1, s2| s1.name.cmp(&s2.name));
        let mut subgraphs_and_enum_values = Vec::new();
        let mut enum_values = IndexSet::default();
        for subgraph in &subgraphs {
            let enum_value = match EnumValue::new(&subgraph.name) {
                Ok(enum_value) => enum_value,
                Err(err) => {
                    self.errors.push(err);
                    continue;
                }
            };

            // Ensure that enum values are unique after normalizing them
            let enum_value = if enum_values.contains(&enum_value.0.to_string()) {
                EnumValue::new(&format!("{}_{}", subgraph.name, enum_values.len()))
                    .expect("adding a suffix always works")
            } else {
                enum_value
            };
            enum_values.insert(enum_value.0.to_string());
            subgraphs_and_enum_values.push((subgraph, enum_value))
        }
        if !self.errors.is_empty() {
            return Err(MergeFailure {
                schema: None,
                composition_hints: self.composition_hints.to_owned(),
                errors: self.errors.to_owned(),
            });
        }

        let mut supergraph = Schema::new();
        // TODO handle @compose

        // add core features
        // TODO verify federation versions across subgraphs
        add_core_feature_link(&mut supergraph);
        add_core_feature_join(&mut supergraph, &subgraphs_and_enum_values);

        // create stubs
        for (subgraph, subgraph_name) in &subgraphs_and_enum_values {
            let sources = Arc::make_mut(&mut supergraph.sources);
            for (key, source) in subgraph.schema.schema().sources.iter() {
                sources.entry(*key).or_insert_with(|| source.clone());
            }

            self.merge_schema(&mut supergraph, subgraph);
            // TODO merge directives

            let metadata = subgraph.schema.metadata();
            let relevant_directives = DirectiveNames::for_metadata(&metadata);

            for (type_name, ty) in &subgraph.schema.schema().types {
                if ty.is_built_in() || !is_mergeable_type(type_name) {
                    // skip built-ins and federation specific types
                    continue;
                }

                match ty {
                    ExtendedType::Enum(value) => self.merge_enum_type(
                        &mut supergraph.types,
                        &relevant_directives,
                        subgraph_name,
                        type_name.clone(),
                        value,
                    ),
                    ExtendedType::InputObject(value) => self.merge_input_object_type(
                        &mut supergraph.types,
                        &relevant_directives,
                        subgraph_name,
                        type_name.clone(),
                        value,
                    ),
                    ExtendedType::Interface(value) => self.merge_interface_type(
                        &mut supergraph.types,
                        &relevant_directives,
                        subgraph_name,
                        type_name.clone(),
                        value,
                    ),
                    ExtendedType::Object(value) => self.merge_object_type(
                        &mut supergraph.types,
                        &relevant_directives,
                        subgraph_name,
                        type_name.clone(),
                        value,
                    ),
                    ExtendedType::Union(value) => self.merge_union_type(
                        &mut supergraph.types,
                        &relevant_directives,
                        subgraph_name,
                        type_name.clone(),
                        value,
                    ),
                    ExtendedType::Scalar(value) => {
                        if !value.is_built_in() {
                            self.merge_scalar_type(
                                &mut supergraph.types,
                                &relevant_directives,
                                subgraph_name.clone(),
                                type_name.clone(),
                                value,
                            );
                        }
                    }
                }
            }

            // merge executable directives
            for (_, directive) in subgraph.schema.schema().directive_definitions.iter() {
                if is_executable_directive(directive) {
                    merge_directive(&mut supergraph.directive_definitions, directive);
                }
            }
        }

        let implementers_map = supergraph.implementers_map();
        self.add_interface_object_fields(&mut supergraph.types, implementers_map)?;

        if self.needs_inaccessible {
            add_core_feature_inaccessible(&mut supergraph);
        }

        if self.errors.is_empty() {
            // TODO: validate here and extend `MergeFailure` to propagate validation errors
            let supergraph = Valid::assume_valid(supergraph);
            Ok(MergeSuccess {
                schema: supergraph,
                composition_hints: self.composition_hints.to_owned(),
            })
        } else {
            Err(MergeFailure {
                schema: Some(Box::new(supergraph)),
                composition_hints: self.composition_hints.to_owned(),
                errors: self.errors.to_owned(),
            })
        }
    }

    fn add_interface_object_fields(
        &mut self,
        types: &mut IndexMap<NamedType, ExtendedType>,
        implementers_map: HashMap<Name, Implementers>,
    ) -> Result<(), MergeFailure> {
        for interface_object_name in self.interface_objects.iter() {
            let Some(ExtendedType::Interface(intf_def)) = types.get(interface_object_name) else {
                return Err(MergeFailure {
                    schema: None,
                    composition_hints: self.composition_hints.to_owned(),
                    errors: vec![format!("Interface {} not found", interface_object_name)],
                });
            };
            let fields = intf_def.fields.clone();

            if let Some(implementers) = implementers_map.get(interface_object_name) {
                for implementer in implementers.iter() {
                    types.entry(implementer.clone()).and_modify(|f| {
                        if let ExtendedType::Object(obj) = f {
                            let obj = obj.make_mut();
                            for (field_name, field_def) in fields.iter() {
                                let mut field_def = field_def.clone();
                                let field_def = field_def.make_mut();
                                field_def.directives = field_def
                                    .directives
                                    .iter()
                                    .filter(|d| d.name != name!("join__field"))
                                    .cloned()
                                    .collect();
                                field_def.directives.push(Node::new(Directive {
                                    name: name!("join__field"),
                                    arguments: vec![],
                                }));

                                obj.fields
                                    .entry(field_name.clone())
                                    .or_insert(field_def.clone().into());
                            }
                        }
                    });
                }
            };
        }

        Ok(())
    }

    fn merge_descriptions<T: Eq + Clone>(&mut self, merged: &mut Option<T>, new: &Option<T>) {
        match (&mut *merged, new) {
            (_, None) => {}
            (None, Some(_)) => merged.clone_from(new),
            (Some(a), Some(b)) => {
                if a != b {
                    // TODO add info about type and from/to subgraph
                    self.composition_hints
                        .push(String::from("conflicting descriptions"));
                }
            }
        }
    }

    fn merge_schema(&mut self, supergraph_schema: &mut Schema, subgraph: &ValidFederationSubgraph) {
        let supergraph_def = &mut supergraph_schema.schema_definition.make_mut();
        let subgraph_def = &subgraph.schema.schema().schema_definition;
        self.merge_descriptions(&mut supergraph_def.description, &subgraph_def.description);

        if subgraph_def.query.is_some() {
            supergraph_def.query.clone_from(&subgraph_def.query);
            // TODO mismatch on query types
        }
        if subgraph_def.mutation.is_some() {
            supergraph_def.mutation.clone_from(&subgraph_def.mutation);
            // TODO mismatch on mutation types
        }
        if subgraph_def.subscription.is_some() {
            supergraph_def
                .subscription
                .clone_from(&subgraph_def.subscription);
            // TODO mismatch on subscription types
        }
    }

    fn merge_enum_type(
        &mut self,
        types: &mut IndexMap<NamedType, ExtendedType>,
        metadata: &DirectiveNames,
        subgraph_name: &EnumValue,
        enum_name: NamedType,
        enum_type: &Node<EnumType>,
    ) {
        let existing_type = types
            .entry(enum_name.clone())
            .or_insert(copy_enum_type(enum_name, enum_type));

        if let ExtendedType::Enum(e) = existing_type {
            let join_type_directives =
                join_type_applied_directive(subgraph_name.clone(), iter::empty(), false);
            e.make_mut().directives.extend(join_type_directives);

            self.add_inaccessible(
                metadata,
                &mut e.make_mut().directives,
                &enum_type.directives,
            );

            self.merge_descriptions(&mut e.make_mut().description, &enum_type.description);

            // TODO we need to merge those fields LAST so we know whether enum is used as input/output/both as different merge rules will apply
            // below logic only works for output enums
            for (enum_value_name, enum_value) in enum_type.values.iter() {
                let ev = e
                    .make_mut()
                    .values
                    .entry(enum_value_name.clone())
                    .or_insert(Component::new(EnumValueDefinition {
                        value: enum_value.value.clone(),
                        description: None,
                        directives: Default::default(),
                    }));
                self.merge_descriptions(&mut ev.make_mut().description, &enum_value.description);

                self.add_inaccessible(
                    metadata,
                    &mut ev.make_mut().directives,
                    &enum_value.directives,
                );

                ev.make_mut().directives.push(Node::new(Directive {
                    name: name!("join__enumValue"),
                    arguments: vec![
                        (Node::new(Argument {
                            name: name!("graph"),
                            value: Node::new(Value::Enum(subgraph_name.to_name())),
                        })),
                    ],
                }));
            }
        } else {
            // TODO - conflict
        }
    }

    fn merge_input_object_type(
        &mut self,
        types: &mut IndexMap<NamedType, ExtendedType>,
        directive_names: &DirectiveNames,
        subgraph_name: &EnumValue,
        input_object_name: NamedType,
        input_object: &Node<InputObjectType>,
    ) {
        let existing_type = types
            .entry(input_object_name.clone())
            .or_insert(copy_input_object_type(input_object_name, input_object));

        if let ExtendedType::InputObject(obj) = existing_type {
            let join_type_directives =
                join_type_applied_directive(subgraph_name.clone(), iter::empty(), false);
            let mutable_object = obj.make_mut();
            mutable_object.directives.extend(join_type_directives);

            self.add_inaccessible(
                directive_names,
                &mut mutable_object.directives,
                &input_object.directives,
            );

            for (field_name, field) in input_object.fields.iter() {
                let existing_field = mutable_object.fields.entry(field_name.clone());

                let supergraph_field = match existing_field {
                    Vacant(i) => i.insert(Component::new(InputValueDefinition {
                        name: field.name.clone(),
                        description: field.description.clone(),
                        ty: field.ty.clone(),
                        default_value: field.default_value.clone(),
                        directives: Default::default(),
                    })),
                    Occupied(i) => {
                        i.into_mut()
                        // merge_options(&i.get_mut().description, &field.description);
                        // TODO check description
                        // TODO check type
                        // TODO check default value
                        // TODO process directives
                    }
                };

                self.add_inaccessible(
                    directive_names,
                    &mut supergraph_field.make_mut().directives,
                    &field.directives,
                );

                let join_field_directive = join_field_applied_directive(
                    subgraph_name,
                    None,
                    None,
                    false,
                    None,
                    Some(&field.ty),
                );
                supergraph_field
                    .make_mut()
                    .directives
                    .push(Node::new(join_field_directive));
            }
        } else {
            // TODO conflict on type
        }
    }

    fn merge_interface_type(
        &mut self,
        types: &mut IndexMap<NamedType, ExtendedType>,
        directive_names: &DirectiveNames,
        subgraph_name: &EnumValue,
        interface_name: NamedType,
        interface: &Node<InterfaceType>,
    ) {
        let existing_type = types
            .entry(interface_name.clone())
            .or_insert(copy_interface_type(interface_name, interface));

        if let ExtendedType::Interface(intf) = existing_type {
            let key_directives = interface.directives.get_all(&directive_names.key);
            let join_type_directives =
                join_type_applied_directive(subgraph_name.clone(), key_directives, false);
            let mutable_intf = intf.make_mut();
            mutable_intf.directives.extend(join_type_directives);

            self.add_inaccessible(
                directive_names,
                &mut mutable_intf.directives,
                &interface.directives,
            );

            interface
                .implements_interfaces
                .iter()
                .for_each(|intf_name| {
                    // IndexSet::insert deduplicates
                    mutable_intf.implements_interfaces.insert(intf_name.clone());
                    let join_implements_directive =
                        join_implements_applied_directive(subgraph_name.clone(), intf_name);
                    mutable_intf.directives.push(join_implements_directive);
                });

            for (field_name, field) in interface.fields.iter() {
                let existing_field = mutable_intf.fields.entry(field_name.clone());
                let supergraph_field = match existing_field {
                    Occupied(f) => {
                        f.into_mut()
                        // TODO check description
                        // TODO check type
                        // TODO check default value
                        // TODO process directives
                    }
                    Vacant(f) => {
                        // TODO warning mismatch missing fields
                        f.insert(Component::new(FieldDefinition {
                            name: field.name.clone(),
                            description: field.description.clone(),
                            arguments: vec![],
                            ty: field.ty.clone(),
                            directives: Default::default(),
                        }))
                    }
                };

                fields::merge_arguments(
                    field.arguments.iter(),
                    &mut supergraph_field.make_mut().arguments,
                    self,
                    directive_names,
                );
                self.merge_descriptions(
                    &mut supergraph_field.make_mut().description,
                    &field.description,
                );

                self.add_inaccessible(
                    directive_names,
                    &mut supergraph_field.make_mut().directives,
                    &field.directives,
                );

                let join_field_directive = join_field_applied_directive(
                    subgraph_name,
                    None,
                    None,
                    false,
                    None,
                    Some(&field.ty),
                );

                supergraph_field
                    .make_mut()
                    .directives
                    .push(Node::new(join_field_directive));
            }
        } else {
            // TODO conflict on type
        }
    }

    fn merge_object_type(
        &mut self,
        types: &mut IndexMap<NamedType, ExtendedType>,
        directive_names: &DirectiveNames,
        subgraph_name: &EnumValue,
        object_name: NamedType,
        object: &Node<ObjectType>,
    ) {
        let is_interface_object = object.directives.has(&directive_names.interface_object);
        let existing_type = types
            .entry(object_name.clone())
            .or_insert(copy_object_type_stub(
                object_name,
                object,
                is_interface_object,
            ));

        if let ExtendedType::Object(obj) = existing_type {
            let key_directives = object.directives.get_all(&directive_names.key);
            let join_type_directives =
                join_type_applied_directive(subgraph_name.clone(), key_directives, false);
            let mutable_object = obj.make_mut();
            mutable_object.directives.extend(join_type_directives);
            self.merge_descriptions(&mut mutable_object.description, &object.description);
            self.add_inaccessible(
                directive_names,
                &mut mutable_object.directives,
                &object.directives,
            );
            object.implements_interfaces.iter().for_each(|intf_name| {
                // IndexSet::insert deduplicates
                mutable_object
                    .implements_interfaces
                    .insert(intf_name.clone());
                let join_implements_directive =
                    join_implements_applied_directive(subgraph_name.clone(), intf_name);
                mutable_object.directives.push(join_implements_directive);
            });

            for (field_name, field) in object.fields.iter() {
                // skip federation built-in queries
                if field_name == "_service" || field_name == "_entities" {
                    continue;
                }

                let existing_field = mutable_object.fields.entry(field_name.clone());
                let supergraph_field = match existing_field {
                    Occupied(f) => {
                        // check description
                        // check type
                        // check args
                        f.into_mut()
                    }
                    Vacant(f) => f.insert(Component::new(FieldDefinition {
                        name: field.name.clone(),
                        description: field.description.clone(),
                        arguments: vec![],
                        directives: Default::default(),
                        ty: field.ty.clone(),
                    })),
                };
                self.merge_descriptions(
                    &mut supergraph_field.make_mut().description,
                    &field.description,
                );

                self.add_inaccessible(
                    directive_names,
                    &mut supergraph_field.make_mut().directives,
                    &field.directives,
                );

                fields::merge_arguments(
                    field.arguments.iter(),
                    &mut supergraph_field.make_mut().arguments,
                    self,
                    directive_names,
                );

                let requires_directive_option = field
                    .directives
                    .get_all(&directive_names.requires)
                    .next()
                    .and_then(|p| directive_string_arg_value(p, &FEDERATION_FIELDS_ARGUMENT_NAME));

                let provides_directive_option = field
                    .directives
                    .get_all(&directive_names.provides)
                    .next()
                    .and_then(|p| directive_string_arg_value(p, &FEDERATION_FIELDS_ARGUMENT_NAME));

                let overrides_directive_option = field
                    .directives
                    .get_all(&directive_names.r#override)
                    .next()
                    .and_then(|p| {
                        let overrides_from =
                            directive_string_arg_value(p, &FEDERATION_FROM_ARGUMENT_NAME);
                        let overrides_label =
                            directive_string_arg_value(p, &FEDERATION_OVERRIDE_LABEL_ARGUMENT_NAME);
                        overrides_from.map(|from| (from, overrides_label))
                    });

                let external_field = field
                    .directives
                    .get_all(&directive_names.external)
                    .next()
                    .is_some();

                let join_field_directive = join_field_applied_directive(
                    subgraph_name,
                    requires_directive_option,
                    provides_directive_option,
                    external_field,
                    overrides_directive_option,
                    Some(&field.ty),
                );

                supergraph_field
                    .make_mut()
                    .directives
                    .push(Node::new(join_field_directive));

                // TODO: implement needsJoinField to avoid adding join__field when unnecessary
                // https://github.com/apollographql/federation/blob/0d8a88585d901dff6844fdce1146a4539dec48df/composition-js/src/merging/merge.ts#L1648
            }
        } else if let ExtendedType::Interface(intf) = existing_type {
            self.interface_objects.insert(intf.name.clone());

            let key_directives = object.directives.get_all(&directive_names.key);
            let join_type_directives =
                join_type_applied_directive(subgraph_name.clone(), key_directives, true);
            let mutable_object = intf.make_mut();
            mutable_object.directives.extend(join_type_directives);
            self.merge_descriptions(&mut mutable_object.description, &object.description);
            self.add_inaccessible(
                directive_names,
                &mut mutable_object.directives,
                &object.directives,
            );

            for (field_name, field) in object.fields.iter() {
                // skip federation built-in queries
                if field_name == "_service" || field_name == "_entities" {
                    continue;
                }

                let existing_field = mutable_object.fields.entry(field_name.clone());
                let supergraph_field = match existing_field {
                    Occupied(f) => {
                        // check description
                        // check type
                        // check args
                        f.into_mut()
                    }
                    Vacant(f) => f.insert(Component::new(FieldDefinition {
                        name: field.name.clone(),
                        description: field.description.clone(),
                        arguments: vec![],
                        directives: Default::default(),
                        ty: field.ty.clone(),
                    })),
                };
                self.merge_descriptions(
                    &mut supergraph_field.make_mut().description,
                    &field.description,
                );

                self.add_inaccessible(
                    directive_names,
                    &mut supergraph_field.make_mut().directives,
                    &field.directives,
                );

                fields::merge_arguments(
                    field.arguments.iter(),
                    &mut supergraph_field.make_mut().arguments,
                    self,
                    directive_names,
                );
                let requires_directive_option = field
                    .directives
                    .get_all(&directive_names.requires)
                    .next()
                    .and_then(|p| directive_string_arg_value(p, &FEDERATION_FIELDS_ARGUMENT_NAME));

                let provides_directive_option = field
                    .directives
                    .get_all(&directive_names.provides)
                    .next()
                    .and_then(|p| directive_string_arg_value(p, &FEDERATION_FIELDS_ARGUMENT_NAME));

                let overrides_directive_option = field
                    .directives
                    .get_all(&directive_names.r#override)
                    .next()
                    .and_then(|p| {
                        let overrides_from =
                            directive_string_arg_value(p, &FEDERATION_FROM_ARGUMENT_NAME);
                        let overrides_label =
                            directive_string_arg_value(p, &FEDERATION_OVERRIDE_LABEL_ARGUMENT_NAME);
                        overrides_from.map(|from| (from, overrides_label))
                    });

                let external_field = field
                    .directives
                    .get_all(&directive_names.external)
                    .next()
                    .is_some();

                let join_field_directive = join_field_applied_directive(
                    subgraph_name,
                    requires_directive_option,
                    provides_directive_option,
                    external_field,
                    overrides_directive_option,
                    Some(&field.ty),
                );

                supergraph_field
                    .make_mut()
                    .directives
                    .push(Node::new(join_field_directive));

                // TODO: implement needsJoinField to avoid adding join__field when unnecessary
                // https://github.com/apollographql/federation/blob/0d8a88585d901dff6844fdce1146a4539dec48df/composition-js/src/merging/merge.ts#L1648
            }
        };
        // TODO merge fields
    }

    fn merge_union_type(
        &mut self,
        types: &mut IndexMap<NamedType, ExtendedType>,
        directive_names: &DirectiveNames,
        subgraph_name: &EnumValue,
        union_name: NamedType,
        union: &Node<UnionType>,
    ) {
        let existing_type = types
            .entry(union_name.clone())
            .or_insert(copy_union_type(union_name, union.description.clone()));

        if let ExtendedType::Union(u) = existing_type {
            let join_type_directives =
                join_type_applied_directive(subgraph_name.clone(), iter::empty(), false);
            u.make_mut().directives.extend(join_type_directives);
            self.add_inaccessible(
                directive_names,
                &mut u.make_mut().directives,
                &union.directives,
            );

            for union_member in union.members.iter() {
                // IndexSet::insert deduplicates
                u.make_mut().members.insert(union_member.clone());
                u.make_mut().directives.push(Component::new(Directive {
                    name: name!("join__unionMember"),
                    arguments: vec![
                        Node::new(Argument {
                            name: name!("graph"),
                            value: Node::new(Value::Enum(subgraph_name.to_name())),
                        }),
                        Node::new(Argument {
                            name: name!("member"),
                            value: union_member.as_str().into(),
                        }),
                    ],
                }));
            }
        }
    }

    fn merge_scalar_type(
        &mut self,
        types: &mut IndexMap<Name, ExtendedType>,
        directive_names: &DirectiveNames,
        subgraph_name: EnumValue,
        scalar_name: NamedType,
        ty: &Node<ScalarType>,
    ) {
        let existing_type = types
            .entry(scalar_name.clone())
            .or_insert(copy_scalar_type(scalar_name, ty));

        if let ExtendedType::Scalar(s) = existing_type {
            let join_type_directives =
                join_type_applied_directive(subgraph_name, iter::empty(), false);
            s.make_mut().directives.extend(join_type_directives);
            self.add_inaccessible(
                directive_names,
                &mut s.make_mut().directives,
                &ty.directives,
            );
        } else {
            // conflict?
        }
    }

    // generic so it handles ast::DirectiveList and schema::DirectiveList
    fn add_inaccessible<I>(
        &mut self,
        directive_names: &DirectiveNames,
        new_directives: &mut Vec<I>,
        original_directives: &[I],
    ) where
        I: AsRef<Directive> + From<Directive> + Clone,
    {
        if original_directives
            .iter()
            .any(|d| d.as_ref().name == directive_names.inaccessible)
            && !new_directives
                .iter()
                .any(|d| d.as_ref().name == INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC)
        {
            self.needs_inaccessible = true;

            new_directives.push(
                Directive {
                    name: INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC,
                    arguments: vec![],
                }
                .into(),
            );
        }
    }
}

struct DirectiveNames {
    key: Name,
    requires: Name,
    provides: Name,
    external: Name,
    interface_object: Name,
    r#override: Name,
    inaccessible: Name,
}

impl DirectiveNames {
    fn for_metadata(metadata: &Option<&LinksMetadata>) -> Self {
        let federation_identity =
            metadata.and_then(|m| m.by_identity.get(&Identity::federation_identity()));

        let key = federation_identity
            .map(|link| link.directive_name_in_schema(&FEDERATION_KEY_DIRECTIVE_NAME_IN_SPEC))
            .unwrap_or(FEDERATION_KEY_DIRECTIVE_NAME_IN_SPEC);

        let requires = federation_identity
            .map(|link| link.directive_name_in_schema(&FEDERATION_REQUIRES_DIRECTIVE_NAME_IN_SPEC))
            .unwrap_or(FEDERATION_REQUIRES_DIRECTIVE_NAME_IN_SPEC);

        let provides = federation_identity
            .map(|link| link.directive_name_in_schema(&FEDERATION_PROVIDES_DIRECTIVE_NAME_IN_SPEC))
            .unwrap_or(FEDERATION_PROVIDES_DIRECTIVE_NAME_IN_SPEC);

        let external = federation_identity
            .map(|link| link.directive_name_in_schema(&FEDERATION_EXTERNAL_DIRECTIVE_NAME_IN_SPEC))
            .unwrap_or(FEDERATION_EXTERNAL_DIRECTIVE_NAME_IN_SPEC);

        let interface_object = federation_identity
            .map(|link| {
                link.directive_name_in_schema(&FEDERATION_INTERFACEOBJECT_DIRECTIVE_NAME_IN_SPEC)
            })
            .unwrap_or(FEDERATION_INTERFACEOBJECT_DIRECTIVE_NAME_IN_SPEC);

        let r#override = federation_identity
            .map(|link| link.directive_name_in_schema(&FEDERATION_OVERRIDE_DIRECTIVE_NAME_IN_SPEC))
            .unwrap_or(FEDERATION_OVERRIDE_DIRECTIVE_NAME_IN_SPEC);

        let inaccessible = federation_identity
            .map(|link| link.directive_name_in_schema(&INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC))
            .unwrap_or(INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC);

        Self {
            key,
            requires,
            provides,
            external,
            interface_object,
            r#override,
            inaccessible,
        }
    }
}

const EXECUTABLE_DIRECTIVE_LOCATIONS: [DirectiveLocation; 8] = [
    DirectiveLocation::Query,
    DirectiveLocation::Mutation,
    DirectiveLocation::Subscription,
    DirectiveLocation::Field,
    DirectiveLocation::FragmentDefinition,
    DirectiveLocation::FragmentSpread,
    DirectiveLocation::InlineFragment,
    DirectiveLocation::VariableDefinition,
];
fn is_executable_directive(directive: &Node<DirectiveDefinition>) -> bool {
    directive
        .locations
        .iter()
        .any(|loc| EXECUTABLE_DIRECTIVE_LOCATIONS.contains(loc))
}

// TODO handle federation specific types - skip if any of the link/fed spec
// TODO this info should be coming from other module
const FEDERATION_TYPES: [&str; 4] = ["_Any", "_Entity", "_Service", "@key"];
fn is_mergeable_type(type_name: &str) -> bool {
    if type_name.starts_with("federation__") || type_name.starts_with("link__") {
        return false;
    }
    !FEDERATION_TYPES.contains(&type_name)
}

fn copy_scalar_type(scalar_name: Name, scalar_type: &Node<ScalarType>) -> ExtendedType {
    ExtendedType::Scalar(Node::new(ScalarType {
        description: scalar_type.description.clone(),
        name: scalar_name,
        directives: Default::default(),
    }))
}

fn copy_enum_type(enum_name: Name, enum_type: &Node<EnumType>) -> ExtendedType {
    ExtendedType::Enum(Node::new(EnumType {
        description: enum_type.description.clone(),
        name: enum_name,
        directives: Default::default(),
        values: IndexMap::default(),
    }))
}

fn copy_input_object_type(
    input_object_name: Name,
    input_object: &Node<InputObjectType>,
) -> ExtendedType {
    let mut new_input_object = InputObjectType {
        description: input_object.description.clone(),
        name: input_object_name,
        directives: Default::default(),
        fields: IndexMap::default(),
    };

    for (field_name, input_field) in input_object.fields.iter() {
        new_input_object.fields.insert(
            field_name.clone(),
            Component::new(InputValueDefinition {
                name: input_field.name.clone(),
                description: input_field.description.clone(),
                directives: Default::default(),
                ty: input_field.ty.clone(),
                default_value: input_field.default_value.clone(),
            }),
        );
    }

    ExtendedType::InputObject(Node::new(new_input_object))
}

fn copy_interface_type(interface_name: Name, interface: &Node<InterfaceType>) -> ExtendedType {
    let new_interface = InterfaceType {
        description: interface.description.clone(),
        name: interface_name,
        directives: Default::default(),
        fields: copy_fields(interface.fields.iter()),
        implements_interfaces: interface.implements_interfaces.clone(),
    };
    ExtendedType::Interface(Node::new(new_interface))
}

fn copy_object_type_stub(
    object_name: Name,
    object: &Node<ObjectType>,
    is_interface_object: bool,
) -> ExtendedType {
    if is_interface_object {
        let new_interface = InterfaceType {
            description: object.description.clone(),
            name: object_name,
            directives: Default::default(),
            fields: copy_fields(object.fields.iter()),
            implements_interfaces: object.implements_interfaces.clone(),
        };
        ExtendedType::Interface(Node::new(new_interface))
    } else {
        let new_object = ObjectType {
            description: object.description.clone(),
            name: object_name,
            directives: Default::default(),
            fields: copy_fields(object.fields.iter()),
            implements_interfaces: object.implements_interfaces.clone(),
        };
        ExtendedType::Object(Node::new(new_object))
    }
}

fn copy_fields(
    fields_to_copy: Iter<Name, Component<FieldDefinition>>,
) -> IndexMap<Name, Component<FieldDefinition>> {
    let mut new_fields: IndexMap<Name, Component<FieldDefinition>> = IndexMap::default();
    for (field_name, field) in fields_to_copy {
        // skip federation built-in queries
        if field_name == "_service" || field_name == "_entities" {
            continue;
        }
        let args: Vec<Node<InputValueDefinition>> = field
            .arguments
            .iter()
            .map(|a| {
                Node::new(InputValueDefinition {
                    name: a.name.clone(),
                    description: a.description.clone(),
                    directives: Default::default(),
                    ty: a.ty.clone(),
                    default_value: a.default_value.clone(),
                })
            })
            .collect();
        let new_field = Component::new(FieldDefinition {
            name: field.name.clone(),
            description: field.description.clone(),
            directives: Default::default(),
            arguments: args,
            ty: field.ty.clone(),
        });

        new_fields.insert(field_name.clone(), new_field);
    }
    new_fields
}

fn copy_union_type(union_name: Name, description: Option<Node<str>>) -> ExtendedType {
    ExtendedType::Union(Node::new(UnionType {
        description,
        name: union_name,
        directives: Default::default(),
        members: IndexSet::default(),
    }))
}

fn join_type_applied_directive<'a>(
    subgraph_name: EnumValue,
    key_directives: impl Iterator<Item = &'a Component<Directive>> + Sized,
    is_interface_object: bool,
) -> Vec<Component<Directive>> {
    let mut join_type_directive = Directive {
        name: name!("join__type"),
        arguments: vec![Node::new(Argument {
            name: name!("graph"),
            value: Node::new(Value::Enum(subgraph_name.into())),
        })],
    };
    if is_interface_object {
        join_type_directive.arguments.push(Node::new(Argument {
            name: name!("isInterfaceObject"),
            value: Node::new(Value::Boolean(is_interface_object)),
        }));
    }

    let mut result = vec![];
    for key_directive in key_directives {
        let mut join_type_directive_with_key = join_type_directive.clone();
        let field_set = directive_string_arg_value(key_directive, &name!("fields")).unwrap();
        join_type_directive_with_key
            .arguments
            .push(Node::new(Argument {
                name: name!("key"),
                value: field_set.into(),
            }));

        let resolvable =
            directive_bool_arg_value(key_directive, &name!("resolvable")).unwrap_or(&true);
        if !resolvable {
            join_type_directive_with_key
                .arguments
                .push(Node::new(Argument {
                    name: name!("resolvable"),
                    value: Node::new(Value::Boolean(false)),
                }));
        }
        result.push(join_type_directive_with_key)
    }
    if result.is_empty() {
        result.push(join_type_directive)
    }
    result
        .into_iter()
        .map(Component::new)
        .collect::<Vec<Component<Directive>>>()
}

fn join_implements_applied_directive(
    subgraph_name: EnumValue,
    intf_name: &Name,
) -> Component<Directive> {
    Component::new(Directive {
        name: name!("join__implements"),
        arguments: vec![
            Node::new(Argument {
                name: name!("graph"),
                value: Node::new(Value::Enum(subgraph_name.into())),
            }),
            Node::new(Argument {
                name: name!("interface"),
                value: intf_name.as_str().into(),
            }),
        ],
    })
}

fn directive_arg_value<'a>(directive: &'a Directive, arg_name: &Name) -> Option<&'a Value> {
    directive
        .arguments
        .iter()
        .find(|arg| arg.name == *arg_name)
        .map(|arg| arg.value.as_ref())
}

fn directive_string_arg_value<'a>(directive: &'a Directive, arg_name: &Name) -> Option<&'a str> {
    match directive_arg_value(directive, arg_name) {
        Some(Value::String(value)) => Some(value),
        _ => None,
    }
}

fn directive_bool_arg_value<'a>(directive: &'a Directive, arg_name: &Name) -> Option<&'a bool> {
    match directive_arg_value(directive, arg_name) {
        Some(Value::Boolean(value)) => Some(value),
        _ => None,
    }
}

// TODO link spec
fn add_core_feature_link(supergraph: &mut Schema) {
    // @link(url: "https://specs.apollo.dev/link/v1.0")
    supergraph
        .schema_definition
        .make_mut()
        .directives
        .push(Component::new(Directive {
            name: name!("link"),
            arguments: vec![Node::new(Argument {
                name: name!("url"),
                value: Node::new("https://specs.apollo.dev/link/v1.0".into()),
            })],
        }));

    let (name, link_purpose_enum) = link_purpose_enum_type();
    supergraph.types.insert(name, link_purpose_enum.into());

    // scalar Import
    let link_import_name = name!("link__Import");
    let link_import_scalar = ExtendedType::Scalar(Node::new(ScalarType {
        directives: Default::default(),
        name: link_import_name.clone(),
        description: None,
    }));
    supergraph
        .types
        .insert(link_import_name, link_import_scalar);

    let link_directive_definition = link_directive_definition();
    supergraph
        .directive_definitions
        .insert(name!("link"), Node::new(link_directive_definition));
}

/// directive @link(url: String, as: String, import: [Import], for: link__Purpose) repeatable on SCHEMA
fn link_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("link"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("url"),
                description: None,
                directives: Default::default(),
                ty: ty!(String).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("as"),
                description: None,
                directives: Default::default(),
                ty: ty!(String).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("for"),
                description: None,
                directives: Default::default(),
                ty: ty!(link__Purpose).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("import"),
                description: None,
                directives: Default::default(),
                ty: ty!([link__Import]).into(),
                default_value: None,
            }),
        ],
        locations: vec![DirectiveLocation::Schema],
        repeatable: true,
    }
}

/// enum link__Purpose {
///   """
///   \`SECURITY\` features provide metadata necessary to securely resolve fields.
///   """
///   SECURITY
///
///   """
///   \`EXECUTION\` features provide metadata necessary for operation execution.
///   """
///   EXECUTION
/// }
fn link_purpose_enum_type() -> (Name, EnumType) {
    let link_purpose_name = name!("link__Purpose");
    let mut link_purpose_enum = EnumType {
        description: None,
        name: link_purpose_name.clone(),
        directives: Default::default(),
        values: IndexMap::default(),
    };
    let link_purpose_security_value = EnumValueDefinition {
        description: Some(
            r"SECURITY features provide metadata necessary to securely resolve fields.".into(),
        ),
        directives: Default::default(),
        value: name!("SECURITY"),
    };
    let link_purpose_execution_value = EnumValueDefinition {
        description: Some(
            r"EXECUTION features provide metadata necessary for operation execution.".into(),
        ),
        directives: Default::default(),
        value: name!("EXECUTION"),
    };
    link_purpose_enum.values.insert(
        link_purpose_security_value.value.clone(),
        Component::new(link_purpose_security_value),
    );
    link_purpose_enum.values.insert(
        link_purpose_execution_value.value.clone(),
        Component::new(link_purpose_execution_value),
    );
    (link_purpose_name, link_purpose_enum)
}

// TODO join spec
fn add_core_feature_join(
    supergraph: &mut Schema,
    subgraphs_and_enum_values: &Vec<(&ValidFederationSubgraph, EnumValue)>,
) {
    // @link(url: "https://specs.apollo.dev/join/v0.5", for: EXECUTION)
    supergraph
        .schema_definition
        .make_mut()
        .directives
        .push(Component::new(Directive {
            name: name!("link"),
            arguments: vec![
                Node::new(Argument {
                    name: name!("url"),
                    value: "https://specs.apollo.dev/join/v0.5".into(),
                }),
                Node::new(Argument {
                    name: name!("for"),
                    value: Node::new(Value::Enum(name!("EXECUTION"))),
                }),
            ],
        }));

    // scalar FieldSet
    let join_field_set_name = name!("join__FieldSet");
    let join_field_set_scalar = ExtendedType::Scalar(Node::new(ScalarType {
        directives: Default::default(),
        name: join_field_set_name.clone(),
        description: None,
    }));
    supergraph
        .types
        .insert(join_field_set_name, join_field_set_scalar);

    // scalar join__FieldValue
    let join_field_value_name = name!("join__FieldValue");
    let join_field_value_scalar = ExtendedType::Scalar(Node::new(ScalarType {
        directives: Default::default(),
        name: join_field_value_name.clone(),
        description: None,
    }));
    supergraph
        .types
        .insert(join_field_value_name, join_field_value_scalar);

    // input join__ContextArgument {
    //   name: String!
    //   type: String!
    //   context: String!
    //   selection: join__FieldValue!
    // }
    let join_context_argument_name = name!("join__ContextArgument");
    let join_context_argument_input = ExtendedType::InputObject(Node::new(InputObjectType {
        description: None,
        name: join_context_argument_name.clone(),
        directives: Default::default(),
        fields: vec![
            (
                name!("name"),
                Component::new(InputValueDefinition {
                    name: name!("name"),
                    description: None,
                    directives: Default::default(),
                    ty: ty!(String!).into(),
                    default_value: None,
                }),
            ),
            (
                name!("type"),
                Component::new(InputValueDefinition {
                    name: name!("type"),
                    description: None,
                    directives: Default::default(),
                    ty: ty!(String!).into(),
                    default_value: None,
                }),
            ),
            (
                name!("context"),
                Component::new(InputValueDefinition {
                    name: name!("context"),
                    description: None,
                    directives: Default::default(),
                    ty: ty!(String!).into(),
                    default_value: None,
                }),
            ),
            (
                name!("selection"),
                Component::new(InputValueDefinition {
                    name: name!("selection"),
                    description: None,
                    directives: Default::default(),
                    ty: ty!(join__FieldValue!).into(),
                    default_value: None,
                }),
            ),
        ]
        .into_iter()
        .collect(),
    }));
    supergraph
        .types
        .insert(join_context_argument_name, join_context_argument_input);

    let join_graph_directive_definition = join_graph_directive_definition();
    supergraph.directive_definitions.insert(
        join_graph_directive_definition.name.clone(),
        Node::new(join_graph_directive_definition),
    );

    let join_type_directive_definition = join_type_directive_definition();
    supergraph.directive_definitions.insert(
        join_type_directive_definition.name.clone(),
        Node::new(join_type_directive_definition),
    );

    let join_field_directive_definition = join_field_directive_definition();
    supergraph.directive_definitions.insert(
        join_field_directive_definition.name.clone(),
        Node::new(join_field_directive_definition),
    );

    let join_implements_directive_definition = join_implements_directive_definition();
    supergraph.directive_definitions.insert(
        join_implements_directive_definition.name.clone(),
        Node::new(join_implements_directive_definition),
    );

    let join_union_member_directive_definition = join_union_member_directive_definition();
    supergraph.directive_definitions.insert(
        join_union_member_directive_definition.name.clone(),
        Node::new(join_union_member_directive_definition),
    );

    let join_enum_value_directive_definition = join_enum_value_directive_definition();
    supergraph.directive_definitions.insert(
        join_enum_value_directive_definition.name.clone(),
        Node::new(join_enum_value_directive_definition),
    );

    // scalar join__DirectiveArguments
    let join_directive_arguments_name = name!("join__DirectiveArguments");
    let join_directive_arguments_scalar = ExtendedType::Scalar(Node::new(ScalarType {
        directives: Default::default(),
        name: join_directive_arguments_name.clone(),
        description: None,
    }));
    supergraph.types.insert(
        join_directive_arguments_name,
        join_directive_arguments_scalar,
    );

    let join_directive_directive_definition = join_directive_directive_definition();
    supergraph.directive_definitions.insert(
        join_directive_directive_definition.name.clone(),
        Node::new(join_directive_directive_definition),
    );

    let (name, join_graph_enum_type) = join_graph_enum_type(subgraphs_and_enum_values);
    supergraph.types.insert(name, join_graph_enum_type.into());
}

/// directive @enumValue(graph: join__Graph!) repeatable on ENUM_VALUE
fn join_enum_value_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__enumValue"),
        description: None,
        arguments: vec![Node::new(InputValueDefinition {
            name: name!("graph"),
            description: None,
            directives: Default::default(),
            ty: ty!(join__Graph!).into(),
            default_value: None,
        })],
        locations: vec![DirectiveLocation::EnumValue],
        repeatable: true,
    }
}

/// directive @join__directive(graphs: [join__Graph!], name: String!, args: join__DirectiveArguments) repeatable on SCHEMA | OBJECT | INTERFACE | FIELD_DEFINITION
fn join_directive_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__directive"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("graphs"),
                description: None,
                directives: Default::default(),
                ty: ty!([join__Graph!]).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("name"),
                description: None,
                directives: Default::default(),
                ty: ty!(String!).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("args"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__DirectiveArguments!).into(),
                default_value: None,
            }),
        ],
        locations: vec![
            DirectiveLocation::Schema,
            DirectiveLocation::Object,
            DirectiveLocation::Interface,
            DirectiveLocation::FieldDefinition,
        ],
        repeatable: true,
    }
}

/// directive @field(
///   graph: Graph,
///   requires: FieldSet,
///   provides: FieldSet,
///   type: String,
///   external: Boolean,
///   override: String,
///   usedOverridden: Boolean
/// ) repeatable on FIELD_DEFINITION | INPUT_FIELD_DEFINITION
fn join_field_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__field"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("graph"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__Graph).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("requires"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__FieldSet).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("provides"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__FieldSet).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("type"),
                description: None,
                directives: Default::default(),
                ty: ty!(String).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("external"),
                description: None,
                directives: Default::default(),
                ty: ty!(Boolean).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("override"),
                description: None,
                directives: Default::default(),
                ty: ty!(String).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: JOIN_OVERRIDE_LABEL_ARGUMENT_NAME,
                description: None,
                directives: Default::default(),
                ty: ty!(String).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("usedOverridden"),
                description: None,
                directives: Default::default(),
                ty: ty!(Boolean).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("contextArguments"),
                description: None,
                directives: Default::default(),
                ty: ty!([join__ContextArgument!]).into(),
                default_value: None,
            }),
        ],
        locations: vec![
            DirectiveLocation::FieldDefinition,
            DirectiveLocation::InputFieldDefinition,
        ],
        repeatable: true,
    }
}

// NOTE: the logic for constructing the contextArguments argument
// is not trivial and is not implemented here. For connectors "expansion",
// it's handled in carryover.rs.
fn join_field_applied_directive(
    subgraph_name: &EnumValue,
    requires: Option<&str>,
    provides: Option<&str>,
    external: bool,
    overrides: Option<(&str, Option<&str>)>, // from, label
    r#type: Option<&Type>,
) -> Directive {
    let mut join_field_directive = Directive {
        name: name!("join__field"),
        arguments: vec![Node::new(Argument {
            name: name!("graph"),
            value: Node::new(Value::Enum(subgraph_name.to_name())),
        })],
    };
    if let Some(required_fields) = requires {
        join_field_directive.arguments.push(Node::new(Argument {
            name: name!("requires"),
            value: required_fields.into(),
        }));
    }
    if let Some(provided_fields) = provides {
        join_field_directive.arguments.push(Node::new(Argument {
            name: name!("provides"),
            value: provided_fields.into(),
        }));
    }
    if external {
        join_field_directive.arguments.push(Node::new(Argument {
            name: name!("external"),
            value: external.into(),
        }));
    }
    if let Some((from, label)) = overrides {
        join_field_directive.arguments.push(Node::new(Argument {
            name: name!("override"),
            value: Node::new(Value::String(from.to_string())),
        }));
        if let Some(label) = label {
            join_field_directive.arguments.push(Node::new(Argument {
                name: name!("overrideLabel"),
                value: Node::new(Value::String(label.to_string())),
            }));
        }
    }
    if let Some(r#type) = r#type {
        join_field_directive.arguments.push(Node::new(Argument {
            name: name!("type"),
            value: r#type.to_string().into(),
        }));
    }
    join_field_directive
}

/// directive @graph(name: String!, url: String!) on ENUM_VALUE
fn join_graph_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__graph"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("name"),
                description: None,
                directives: Default::default(),
                ty: ty!(String!).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("url"),
                description: None,
                directives: Default::default(),
                ty: ty!(String!).into(),
                default_value: None,
            }),
        ],
        locations: vec![DirectiveLocation::EnumValue],
        repeatable: false,
    }
}

/// directive @implements(
///   graph: Graph!,
///   interface: String!
/// ) on OBJECT | INTERFACE
fn join_implements_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__implements"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("graph"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__Graph!).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("interface"),
                description: None,
                directives: Default::default(),
                ty: ty!(String!).into(),
                default_value: None,
            }),
        ],
        locations: vec![DirectiveLocation::Interface, DirectiveLocation::Object],
        repeatable: true,
    }
}

/// directive @type(
///   graph: Graph!,
///   key: FieldSet,
///   extension: Boolean! = false,
///   resolvable: Boolean = true,
///   isInterfaceObject: Boolean = false
/// ) repeatable on OBJECT | INTERFACE | UNION | ENUM | INPUT_OBJECT | SCALAR
fn join_type_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__type"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("graph"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__Graph!).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("key"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__FieldSet).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("extension"),
                description: None,
                directives: Default::default(),
                ty: ty!(Boolean!).into(),
                default_value: Some(Node::new(Value::Boolean(false))),
            }),
            Node::new(InputValueDefinition {
                name: name!("resolvable"),
                description: None,
                directives: Default::default(),
                ty: ty!(Boolean!).into(),
                default_value: Some(Node::new(Value::Boolean(true))),
            }),
            Node::new(InputValueDefinition {
                name: name!("isInterfaceObject"),
                description: None,
                directives: Default::default(),
                ty: ty!(Boolean!).into(),
                default_value: Some(Node::new(Value::Boolean(false))),
            }),
        ],
        locations: vec![
            DirectiveLocation::Enum,
            DirectiveLocation::InputObject,
            DirectiveLocation::Interface,
            DirectiveLocation::Object,
            DirectiveLocation::Scalar,
            DirectiveLocation::Union,
        ],
        repeatable: true,
    }
}

/// directive @unionMember(graph: join__Graph!, member: String!) repeatable on UNION
fn join_union_member_directive_definition() -> DirectiveDefinition {
    DirectiveDefinition {
        name: name!("join__unionMember"),
        description: None,
        arguments: vec![
            Node::new(InputValueDefinition {
                name: name!("graph"),
                description: None,
                directives: Default::default(),
                ty: ty!(join__Graph!).into(),
                default_value: None,
            }),
            Node::new(InputValueDefinition {
                name: name!("member"),
                description: None,
                directives: Default::default(),
                ty: ty!(String!).into(),
                default_value: None,
            }),
        ],
        locations: vec![DirectiveLocation::Union],
        repeatable: true,
    }
}

/// enum Graph
fn join_graph_enum_type(
    subgraphs_and_enum_values: &Vec<(&ValidFederationSubgraph, EnumValue)>,
) -> (Name, EnumType) {
    let join_graph_enum_name = name!("join__Graph");
    let mut join_graph_enum_type = EnumType {
        description: None,
        name: join_graph_enum_name.clone(),
        directives: Default::default(),
        values: IndexMap::default(),
    };
    for (s, subgraph_name) in subgraphs_and_enum_values {
        let join_graph_applied_directive = Directive {
            name: name!("join__graph"),
            arguments: vec![
                (Node::new(Argument {
                    name: name!("name"),
                    value: s.name.as_str().into(),
                })),
                (Node::new(Argument {
                    name: name!("url"),
                    value: s.url.as_str().into(),
                })),
            ],
        };
        let graph = EnumValueDefinition {
            description: None,
            directives: DirectiveList(vec![Node::new(join_graph_applied_directive)]),
            value: subgraph_name.to_name(),
        };
        join_graph_enum_type
            .values
            .insert(graph.value.clone(), Component::new(graph));
    }
    (join_graph_enum_name, join_graph_enum_type)
}

/// Represents a valid enum value in GraphQL, used for building `join__Graph`.
///
/// TODO: Put this in `join_spec_definition.rs` when we convert to using that module.
#[derive(Clone, Debug)]
struct EnumValue(Name);

impl EnumValue {
    fn new(raw: &str) -> Result<Self, String> {
        let prefix = if raw.starts_with(char::is_numeric) {
            Some('_')
        } else {
            None
        };
        let name = prefix
            .into_iter()
            .chain(raw.chars())
            .map(|c| match c {
                'a'..='z' => c.to_ascii_uppercase(),
                'A'..='Z' | '0'..='9' => c,
                _ => '_',
            })
            .collect::<String>();
        Name::new(&name)
            .map(Self)
            .map_err(|_| format!("Failed to transform {raw} into a valid GraphQL name. Got {name}"))
    }
    fn to_name(&self) -> Name {
        self.0.clone()
    }

    #[cfg(test)]
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<EnumValue> for Name {
    fn from(ev: EnumValue) -> Self {
        ev.0
    }
}

#[cfg(test)]
mod test_enum_value {
    #[test]
    fn basic() {
        let ev = super::EnumValue::new("subgraph").unwrap();
        assert_eq!(ev.as_str(), "SUBGRAPH");
    }

    #[test]
    fn with_underscores() {
        let ev = super::EnumValue::new("a_subgraph").unwrap();
        assert_eq!(ev.as_str(), "A_SUBGRAPH");
    }

    #[test]
    fn with_hyphens() {
        let ev = super::EnumValue::new("a-subgraph").unwrap();
        assert_eq!(ev.as_str(), "A_SUBGRAPH");
    }

    #[test]
    fn special_symbols() {
        let ev = super::EnumValue::new("a$ubgraph").unwrap();
        assert_eq!(ev.as_str(), "A_UBGRAPH");
    }

    #[test]
    fn digit_first_char() {
        let ev = super::EnumValue::new("1subgraph").unwrap();
        assert_eq!(ev.as_str(), "_1SUBGRAPH");
    }

    #[test]
    fn digit_last_char() {
        let ev = super::EnumValue::new("subgraph_1").unwrap();
        assert_eq!(ev.as_str(), "SUBGRAPH_1");
    }
}

fn add_core_feature_inaccessible(supergraph: &mut Schema) {
    // @link(url: "https://specs.apollo.dev/inaccessible/v0.2")
    let spec = InaccessibleSpecDefinition::new(
        Version { major: 0, minor: 2 },
        Version { major: 2, minor: 0 },
    );

    supergraph
        .schema_definition
        .make_mut()
        .directives
        .push(Component::new(Directive {
            name: name!("link"),
            arguments: vec![
                Node::new(Argument {
                    name: name!("url"),
                    value: spec.to_string().into(),
                }),
                Node::new(Argument {
                    name: name!("for"),
                    value: Node::new(Value::Enum(name!("SECURITY"))),
                }),
            ],
        }));

    supergraph.directive_definitions.insert(
        INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC,
        Node::new(DirectiveDefinition {
            name: INACCESSIBLE_DIRECTIVE_NAME_IN_SPEC,
            description: None,
            arguments: vec![],
            locations: vec![
                DirectiveLocation::FieldDefinition,
                DirectiveLocation::Object,
                DirectiveLocation::Interface,
                DirectiveLocation::Union,
                DirectiveLocation::ArgumentDefinition,
                DirectiveLocation::Scalar,
                DirectiveLocation::Enum,
                DirectiveLocation::EnumValue,
                DirectiveLocation::InputObject,
                DirectiveLocation::InputFieldDefinition,
            ],
            repeatable: false,
        }),
    );
}

fn merge_directive(
    supergraph_directives: &mut IndexMap<Name, Node<DirectiveDefinition>>,
    directive: &Node<DirectiveDefinition>,
) {
    if !supergraph_directives.contains_key(&directive.name.clone()) {
        supergraph_directives.insert(directive.name.clone(), directive.clone());
    }
}

#[cfg(test)]
mod tests;
