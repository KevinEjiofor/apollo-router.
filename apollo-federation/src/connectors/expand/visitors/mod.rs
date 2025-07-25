//! Expansion Visitors
//!
//! This module contains various helper visitors for traversing nested structures,
//! adding needed types to a mutable schema.

pub(crate) mod input;
mod selection;

use std::collections::VecDeque;

use apollo_compiler::Name;
use apollo_compiler::ast::Directive;
use indexmap::IndexSet;

use crate::schema::FederationSchema;
use crate::schema::ValidFederationSchema;

/// Filter out directives from a directive list
pub(crate) fn filter_directives<'a, D, I, O>(deny_list: &IndexSet<Name>, directives: D) -> O
where
    D: IntoIterator<Item = &'a I>,
    I: 'a + AsRef<Directive> + Clone,
    O: FromIterator<I>,
{
    directives
        .into_iter()
        .filter(|d| !deny_list.contains(&d.as_ref().name))
        .cloned()
        .collect()
}

/// Try to pre-insert into a schema, ignoring the operation if the type already exists
/// and matches the existing type
macro_rules! try_pre_insert {
    ($schema:expr, $pos:expr) => {{
        if let Some(old_pos) = $schema.try_get_type($pos.type_name.clone()) {
            // Verify that the types match
            let pos = $crate::schema::position::TypeDefinitionPosition::from($pos.clone());
            if old_pos != pos {
                Err($crate::FederationError::internal(format!(
                    "found different type when upserting: expected {:?} found {:?}",
                    pos, old_pos
                )))
            } else {
                Ok(())
            }
        } else {
            $pos.pre_insert($schema)
        }
    }};
}

/// Try to insert into a schema, ignoring the operation if the type already exists
/// and matches the existing type
macro_rules! try_insert {
    ($schema:expr, $pos:expr, $def:expr) => {{
        if let Some(old_pos) = $schema.try_get_type($pos.type_name.clone()) {
            // Verify that the types match
            let pos = $crate::schema::position::TypeDefinitionPosition::from($pos.clone());
            if old_pos != pos {
                Err($crate::FederationError::internal(format!(
                    "found different type when upserting: expected {:?} found {:?}",
                    pos, old_pos
                )))
            } else {
                Ok(())
            }
        } else {
            $pos.insert($schema, $def)
        }
    }};
}
pub(crate) use try_insert;
pub(crate) use try_pre_insert;

/// Visitor for arbitrary field types.
///
/// Any type of interest that should be viewed when traversing the tree-like structure
/// defined by [GroupVisitor] should implement this trait.
pub(crate) trait FieldVisitor<Field>: Sized {
    type Error;

    /// Visit a field
    fn visit(&mut self, field: Field) -> Result<(), Self::Error>;
}

/// Visitor for arbitrary tree-like structures where nodes can also have children
///
/// This trait treats all nodes in the graph as Fields, checking if a Field is also
/// a group for handling children. Visiting order is depth-first.
pub(crate) trait GroupVisitor<Group, Field>
where
    Self: FieldVisitor<Field>,
    Field: Clone,
{
    /// Try to get a group from a field, returning None if the field is not a group
    fn try_get_group_for_field(
        &self,
        field: &Field,
    ) -> Result<Option<Group>, <Self as FieldVisitor<Field>>::Error>;

    /// Enter a subselection group
    /// Note: You can assume that the field corresponding to this
    /// group will be visited first.
    fn enter_group(
        &mut self,
        group: &Group,
    ) -> Result<Vec<Field>, <Self as FieldVisitor<Field>>::Error>;

    /// Exit a subselection group
    /// Note: You can assume that the named selection corresponding to this
    /// group will be visited and entered first.
    fn exit_group(&mut self) -> Result<(), <Self as FieldVisitor<Field>>::Error>;

    /// Walk through the `Group`, visiting each output key. If at any point, one of the
    /// visitor methods returns an error, then the walk will be stopped and the error will be
    /// returned.
    fn walk(mut self, entry: Group) -> Result<Self, <Self as FieldVisitor<Field>>::Error> {
        // Start visiting each of the fields
        let mut to_visit =
            VecDeque::from_iter(self.enter_group(&entry)?.into_iter().map(|n| (0i32, n)));
        let mut current_depth = 0;
        while let Some((depth, next)) = to_visit.pop_front() {
            for _ in depth..current_depth {
                self.exit_group()?;
            }
            current_depth = depth;

            self.visit(next.clone())?;

            // If we have a named selection that has a subselection, then we want to
            // make sure that we visit the children before all other siblings.
            //
            // Note: We reverse here since we always push to the front.
            if let Some(group) = self.try_get_group_for_field(&next)? {
                current_depth += 1;

                let fields = self.enter_group(&group)?;
                fields
                    .into_iter()
                    .rev()
                    .for_each(|s| to_visit.push_front((current_depth, s)));
            }
        }

        // Make sure that we exit until we are no longer nested
        for _ in 0..=current_depth {
            self.exit_group()?;
        }

        Ok(self)
    }
}

/// A visitor for schema building.
///
/// This implementation of the JSONSelection visitor walks a JSONSelection,
/// copying over all output types (and respective fields / sub types) as it goes
/// from a reference schema.
pub(crate) struct SchemaVisitor<'a, Group, GroupType> {
    /// List of directives to not copy over into the target schema.
    directive_deny_list: &'a IndexSet<Name>,

    /// The original schema used for sourcing all types / fields / directives / etc.
    original_schema: &'a ValidFederationSchema,

    /// The target schema for adding all types.
    to_schema: &'a mut FederationSchema,

    /// A stack of parent types used for fetching subtypes
    ///
    /// Each entry corresponds to a nested subselect in the JSONSelection.
    type_stack: Vec<(Group, GroupType)>,
}

impl<'a, Group, GroupType> SchemaVisitor<'a, Group, GroupType> {
    pub(crate) fn new(
        original_schema: &'a ValidFederationSchema,
        to_schema: &'a mut FederationSchema,
        directive_deny_list: &'a IndexSet<Name>,
    ) -> Self {
        SchemaVisitor {
            directive_deny_list,
            original_schema,
            to_schema,
            type_stack: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use itertools::Itertools;

    use crate::connectors::JSONSelection;
    use crate::connectors::SubSelection;
    use crate::connectors::expand::visitors::FieldVisitor;
    use crate::connectors::expand::visitors::GroupVisitor;
    use crate::connectors::json_selection::NamedSelection;
    use crate::error::FederationError;

    /// Visitor for tests.
    ///
    /// Each node visited is added, along with its depth. This is later printed
    /// such that groups are indented based on depth.
    struct TestVisitor<'a> {
        depth_stack: Vec<usize>,
        visited: &'a mut Vec<(usize, String)>,
    }

    impl<'a> TestVisitor<'a> {
        fn new(visited: &'a mut Vec<(usize, String)>) -> Self {
            Self {
                depth_stack: Vec::new(),
                visited,
            }
        }

        fn last_depth(&self) -> Option<usize> {
            self.depth_stack.last().copied()
        }
    }

    fn print_visited(visited: Vec<(usize, String)>) -> String {
        let mut result = String::new();
        for (depth, visited) in visited {
            result.push_str(&format!("{}{visited}\n", "|  ".repeat(depth)));
        }

        result
    }

    impl FieldVisitor<NamedSelection> for TestVisitor<'_> {
        type Error = FederationError;

        fn visit<'a>(&mut self, field: NamedSelection) -> Result<(), Self::Error> {
            for name in field.names() {
                self.visited
                    .push((self.last_depth().unwrap_or_default(), name.to_string()));
            }

            Ok(())
        }
    }

    impl GroupVisitor<SubSelection, NamedSelection> for TestVisitor<'_> {
        fn try_get_group_for_field(
            &self,
            field: &NamedSelection,
        ) -> Result<Option<SubSelection>, FederationError> {
            Ok(field.next_subselection().cloned())
        }

        fn enter_group(
            &mut self,
            group: &SubSelection,
        ) -> Result<Vec<NamedSelection>, FederationError> {
            let next_depth = self.last_depth().map(|d| d + 1).unwrap_or(0);
            self.depth_stack.push(next_depth);
            Ok(group
                .selections_iter()
                .sorted_by_key(|s| s.names())
                .cloned()
                .collect())
        }

        fn exit_group(&mut self) -> Result<(), FederationError> {
            self.depth_stack.pop().unwrap();
            Ok(())
        }
    }

    #[test]
    fn it_iterates_over_empty_path() {
        let mut visited = Vec::new();
        let visitor = TestVisitor::new(&mut visited);
        let selection = JSONSelection::parse("").unwrap();

        visitor
            .walk(selection.next_subselection().cloned().unwrap())
            .unwrap();
        assert_snapshot!(print_visited(visited), @"");
    }

    #[test]
    fn it_iterates_over_simple_selection() {
        let mut visited = Vec::new();
        let visitor = TestVisitor::new(&mut visited);
        let selection = JSONSelection::parse("a b c d").unwrap();

        visitor
            .walk(selection.next_subselection().cloned().unwrap())
            .unwrap();
        assert_snapshot!(print_visited(visited), @r###"
        a
        b
        c
        d
        "###);
    }

    #[test]
    fn it_iterates_over_aliased_selection() {
        let mut visited = Vec::new();
        let visitor = TestVisitor::new(&mut visited);
        let selection = JSONSelection::parse("a: one b: two c: three d: four").unwrap();

        visitor
            .walk(selection.next_subselection().cloned().unwrap())
            .unwrap();
        assert_snapshot!(print_visited(visited), @r###"
        a
        b
        c
        d
        "###);
    }

    #[test]
    fn it_iterates_over_nested_selection() {
        let mut visited = Vec::new();
        let visitor = TestVisitor::new(&mut visited);
        let selection = JSONSelection::parse("a { b { c { d { e } } } } f").unwrap();

        visitor
            .walk(selection.next_subselection().cloned().unwrap())
            .unwrap();
        assert_snapshot!(print_visited(visited), @r###"
        a
        |  b
        |  |  c
        |  |  |  d
        |  |  |  |  e
        f
        "###);
    }

    #[test]
    fn it_iterates_over_paths() {
        let mut visited = Vec::new();
        let visitor = TestVisitor::new(&mut visited);
        let selection = JSONSelection::parse(
            "a
            $.b {
                c
                $.d {
                    e
                    f: g.h { i }
                }
            }
            j",
        )
        .unwrap();

        visitor
            .walk(selection.next_subselection().cloned().unwrap())
            .unwrap();
        assert_snapshot!(print_visited(visited), @r###"
        a
        c
        e
        f
        |  i
        j
        "###);
    }

    #[test]
    fn it_iterates_over_complex_selection() {
        let mut visited = Vec::new();
        let visitor = TestVisitor::new(&mut visited);
        let selection = JSONSelection::parse(
            "id
            name
            username
            email
            address {
              street
              suite
              city
              zipcode
              geo {
                lat
                lng
              }
            }
            phone
            website
            company {
              name
              catchPhrase
              bs
            }",
        )
        .unwrap();

        visitor
            .walk(selection.next_subselection().cloned().unwrap())
            .unwrap();
        assert_snapshot!(print_visited(visited), @r###"
        address
        |  city
        |  geo
        |  |  lat
        |  |  lng
        |  street
        |  suite
        |  zipcode
        company
        |  bs
        |  catchPhrase
        |  name
        email
        id
        name
        phone
        username
        website
        "###);
        // let iter = selection.iter();
        // assert_debug_snapshot!(iter.collect_vec());
    }
}
