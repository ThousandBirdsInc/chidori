use std::collections::{HashMap, HashSet};
use std::mem;

use anyhow::anyhow;
use apollo_compiler::ApolloCompiler;
use apollo_encoder::{Document as EncoderDocument, Document, FieldDefinition, Selection, SelectionSet, Field as EncoderField};
use apollo_parser::Parser as ApolloParser;
use apollo_parser::ast::{AstNode, Field, Value};
use indoc::indoc;
use petgraph::dot::{Config, Dot};
use petgraph::graphmap::DiGraphMap;
use sqlparser::ast::{SetExpr, Statement};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::graph_definition::DefinitionGraph;
use crate::proto2::{File, Item, item as dsl_item, ItemCore};
use crate::utils;
use crate::utils::wasm_error::CoreError;


/// Takes a string for a graphql output type and returns an encoded apollo document
pub fn parse_graphql_type_def(name: &str, output_type: &str) -> anyhow::Result<EncoderDocument> {
    let parser = ApolloParser::new(output_type).recursion_limit(100);
    let doc = parser.parse().document();
    let mut encoder = EncoderDocument::try_from(doc).unwrap();
    if let Some(encoder_def) = encoder.object_type_definitions.first() {
        let new_type_def = apollo_encoder::ObjectDefinition {
            name: utils::uppercase_first_letter(name),
            description: encoder_def.description.clone(),
            directives: encoder_def.directives.clone(),
            fields: encoder_def.fields.clone(),
            interfaces: encoder_def.interfaces.clone(),
            extend: encoder_def.extend,
        };
        encoder.object_type_definitions = vec![new_type_def];
    } else {
        return Err(anyhow!("Object type definition for node is missing"));
    }
    return Ok(encoder)
}

/// From an Item record, extracts the output type definition and returns it as an encoded apollo document
pub fn extract_output_types(item: &Item) -> anyhow::Result<EncoderDocument> {
    let name = &item.core.as_ref().unwrap().name;
    let output_type = &item.core.as_ref().unwrap().output.as_ref().unwrap().output;
    let doc = parse_graphql_type_def(name, &output_type)?;
    Ok(doc)
}


/// Generate our GQL schema for a given set of node query types. Renames the name of the query to the Title Case name of the
/// the node.
fn generate_gql_schema_query_type(node_output_type_docs: &HashMap<String, EncoderDocument>) -> EncoderDocument {
    let mut object_def = apollo_encoder::ObjectDefinition::new("Query".to_string());
    for (name, _node_output_type_doc) in node_output_type_docs.iter() {
        let ty = apollo_encoder::Type_::NamedType { name: utils::uppercase_first_letter(&name), };
        let field = apollo_encoder::FieldDefinition::new(utils::lowercase_first_letter(name), ty);
        object_def.field(field);
    }
    let mut query_doc = EncoderDocument::new();
    query_doc.object(object_def);
    query_doc
}

/// Accepts a string for a graphql query and returns an encoded apollo document
pub fn parse_graphql_query_def(name: &str, query: &str) -> anyhow::Result<EncoderDocument> {
    let parser = ApolloParser::new(query).recursion_limit(100);
    let ast = parser.parse();
    let doc = ast.document();
    let mut encoder = EncoderDocument::try_from(doc).unwrap();
    if let Some(encoder_def) = encoder.operation_definitions.first() {
        let new_op_def = apollo_encoder::OperationDefinition {
            operation_type: apollo_encoder::OperationType::Query,
            name: Some(utils::uppercase_first_letter(name)),
            variable_definitions: encoder_def.variable_definitions.clone(),
            directives: encoder_def.directives.clone(),
            selection_set: encoder_def.selection_set.clone(),
            shorthand: encoder_def.shorthand,
        };
        encoder.operation_definitions = vec![new_op_def];
    } else {
        return Err(anyhow!("Object type definition for node {} is missing", name));
    }
    Ok(encoder)
}

/// From an Item record, extracts the query type definitions and returns them as encoded apollo documents
pub fn extract_query_types(item_core: &ItemCore) -> anyhow::Result<Vec<Option<EncoderDocument>>> {
    let name = &item_core.name;
    let query_docs: Result<Vec<Option<EncoderDocument>>, _>  = item_core.queries.clone().into_iter().map(|query_type| {
        query_type.query.map(|q| {
            parse_graphql_query_def(name, &q)
        }).transpose()
    }).collect();
    query_docs
}


pub fn parse_where_query(input: &str) {
    // We prepend an arbitrary SELECT * FROM x to the where so its a valid statement
    // and then ignore that because we're only interested in the where clause
    let mut sql = String::from("SELECT * FROM x ");
    sql.push_str(input);

    // Parse the sql, and then extract the where clause AST
    let dialect = GenericDialect {};
    let ast = Parser::parse_sql(&dialect, &sql).unwrap();
    for stmt in ast {
        if let Statement::Query(query) = stmt {
            if let SetExpr::Select(select) = *query.body {
                if let Some(where_clause) = select.selection {
                    println!("Where Clause: {:?}", where_clause);
                }
            }
        }
    }
}

// TODO: using a parsed output type, generate a maximal operation query definition
fn generate_maximal_operation_def_from_output() {
    unimplemented!();
}

fn get_selection_set_paths(selection_set: &SelectionSet) -> Vec<Vec<String>> {
    let mut paths = vec![];
    for selection in &selection_set.selections {
        match selection {
            Selection::Field(field) => {
                let this_path = vec![field.name.clone()];
                if let Some(selection_set) = &field.selection_set {
                    for path in get_selection_set_paths(selection_set) {
                        let mut new_path = this_path.clone();
                        new_path.extend(path);
                        paths.push(new_path);
                    }
                } else {
                    paths.push(this_path);
                }
            }
            Selection::FragmentSpread(_) => {}
            Selection::InlineFragment(_) => {}
        }
    }
    paths
}

pub fn get_paths_for_query(query_encoder_doc: &EncoderDocument) -> Vec<Vec<String>> {
    let mut paths = vec![];
    query_encoder_doc.operation_definitions.iter().for_each(|op_def| {
        for field_name_path in get_selection_set_paths(&op_def.selection_set) {
            paths.push(field_name_path);
        }
    });
    paths
}

/// Converts an output type ApolloEncoderDocument into a shredded list of resolved paths
/// as Vec<String> for each path.
pub fn get_paths_for_output(output_tables: &HashSet<String>, output_encoder_doc: &EncoderDocument) -> OutputPath {
    let mut path_per_value = vec![];
    output_encoder_doc.object_type_definitions.iter().for_each(|op_def| {
        for field in &op_def.fields {
            for table in output_tables.iter() {
                path_per_value.push(vec![table.clone(), field.name.clone()]);
            }
        }
    });
    path_per_value
}


/// Build a map of output paths to the nodes that they refer to
pub fn output_table_from_output_types(output_paths: &HashMap<String, Vec<Vec<String>>>) -> HashMap<String, Vec<String>> {
    let mut result: HashMap<Vec<String>, Vec<String>> = HashMap::new();

    for (key, paths) in output_paths.iter() {
        for path in paths {
            result.entry(path.clone())
                .or_insert(vec![])
                .push(key.clone());
        }
    }


    // Mutate the result so all of the keys are flat
    result
        .into_iter()
        .map(|(k, v)| (k.join(":"), v))
        .collect()
}

/// Build a map of query paths to the nodes they refer to
pub fn dispatch_table_from_query_paths(query_paths: &HashMap<String, Vec<Option<QueryVecGroup>>>) -> HashMap<String, Vec<String>> {
    let mut result: HashMap<Vec<String>, Vec<String>> = HashMap::new();

    for (key, all_opt_paths) in query_paths.iter() {
        for opt_paths in all_opt_paths {
            if let Some(paths) = opt_paths {
                for path in paths {
                    result.entry(path.clone())
                        .or_insert(vec![])
                        .push(key.clone());
                }
            } else {
                result.entry(vec![])
                    .or_insert(vec![])
                    .push(key.clone());
            }
        }
    }


    // Mutate the result so all of the keys are flat
    result
        .into_iter()
        .map(|(k, v)| (k.join(":"), v))
        .collect()
}

fn parse_field_arguments(field: Field) {
    field.arguments().iter().for_each(|arg| {
        arg.arguments().for_each(|a| {
            if let Some(val) = a.value() {
                match val {
                    Value::Variable(_) => {}
                    Value::StringValue(v) => {
                        println!("argument {:?} = {:?}",
                                 a.name().unwrap().text().to_string(),
                                 v.source_string()
                        );
                    }
                    Value::FloatValue(_) => {}
                    Value::IntValue(v) => {
                        println!("argument {:?} = {:?}",
                                 a.name().unwrap().text().to_string(),
                                 v.source_string()
                        );

                    }
                    Value::BooleanValue(_) => {}
                    Value::NullValue(_) => {}
                    Value::EnumValue(_) => {}
                    Value::ListValue(_) => {}
                    Value::ObjectValue(_) => {}
                }
            }
        });
    });
}


#[derive(Debug, Clone)]
pub struct CleanIndividualNode {
    pub name: String,
    query_type: QueryType,
    query_path: QueryPath,
    pub output_path: OutputPath,
    output_type: EncoderDocument,
    output_table: HashSet<String>,
}

pub fn derive_for_individual_node(node: &Item) -> anyhow::Result<CleanIndividualNode> {
    let name = &node.core.as_ref().unwrap().name;
    let core = node.core.as_ref().unwrap();

    // Add the node to the output table
    let mut output_table: HashSet<_> = core.output_tables.iter().cloned().collect();
    output_table.insert(core.name.clone());

    let query_type = extract_query_types(core)?;
    let output_type = extract_output_types(&node)?;
    let query_path = query_type.iter().map(|x| x.as_ref().map(get_paths_for_query)).collect();
    let output_path = get_paths_for_output(&output_table, &output_type);
    Ok(CleanIndividualNode {
        name: name.clone(),
        query_type,
        query_path,
        output_path,
        output_type,
        output_table
    })
}


type QueryType =  Vec<Option<EncoderDocument>>;
type QueryVecGroup = Vec<Vec<String>>;
type QueryPath =  Vec<Option<QueryVecGroup>>;
type OutputPath =  Vec<Vec<String>>;


#[derive(Debug, Clone)]
pub struct CleanedDefinitionGraph {
    pub query_types: HashMap<String, QueryType>,
    pub query_paths: HashMap<String, QueryPath>,
    pub output_types: HashMap<String, EncoderDocument>,
    pub node_by_name: HashMap<String, Item>,
    pub dispatch_table: HashMap<String, Vec<String>>,
    pub output_table: HashMap<String, Vec<String>>,
    pub node_to_output_tables: HashMap<String, HashSet<String>>,
    pub gql_query_type: EncoderDocument,
    pub output_paths: HashMap<String, OutputPath>,
    pub unified_type_doc: EncoderDocument,

}

impl CleanedDefinitionGraph {
    pub fn new(definition_graph: &DefinitionGraph) -> Self {
        let node_by_name = definition_graph.get_nodes().iter().map(|n| {
            let name = &n.core.as_ref().unwrap().name;
            (name.clone(), n.clone())
        }).collect();

        CleanedDefinitionGraph::recompute_parsed_values(node_by_name).unwrap()
    }

    pub fn zero() -> Self {
        Self {
            query_types: HashMap::new(),
            query_paths: HashMap::new(),
            output_table: HashMap::new(),
            dispatch_table: HashMap::new(),
            output_types: HashMap::new(),
            output_paths: HashMap::new(),
            node_by_name: HashMap::new(),
            gql_query_type: EncoderDocument::new(),
            unified_type_doc: EncoderDocument::new(),
            node_to_output_tables: HashMap::new(),
        }
    }

    pub fn get_node(&self, name: &str) -> Option<&Item> {
        self.node_by_name.get(name)
    }

    fn recompute_parsed_values(node_by_name: HashMap<String, Item>) -> anyhow::Result<CleanedDefinitionGraph> {
        // Node name -> list of encoder documents
        let mut graph = CleanedDefinitionGraph::zero();

        for node in node_by_name.values() {
            let indiv = &mut derive_for_individual_node(node)?;
            let name = &indiv.name;
            graph.node_to_output_tables.insert(name.clone(), mem::take(&mut indiv.output_table));
            graph.query_types.insert(name.clone(), mem::take(&mut indiv.query_type));
            graph.output_types.insert(name.clone(), mem::take(&mut indiv.output_type));
            graph.query_paths.insert(name.clone(), mem::take(&mut indiv.query_path));
            graph.output_paths.insert(name.clone(), mem::take(&mut indiv.output_path));
        }

        // This aggregates all the query types into a single query type
        graph.gql_query_type = generate_gql_schema_query_type(&graph.output_types);
        graph.output_table = output_table_from_output_types(&graph.output_paths);
        graph.dispatch_table = dispatch_table_from_query_paths(&graph.query_paths);
        graph.unified_type_doc = build_type_document(graph.output_types.iter().collect(), &graph.gql_query_type);
        graph.node_by_name = node_by_name;

        Ok(graph)
    }

    pub fn assert_parsing(&mut self) -> anyhow::Result<()> {
        let recomputed = CleanedDefinitionGraph::recompute_parsed_values(
            mem::take(&mut self.node_by_name)
        ).unwrap();
        self.node_by_name = recomputed.node_by_name;
        self.query_types = recomputed.query_types;
        self.query_paths = recomputed.query_paths;
        self.output_types = recomputed.output_types;
        self.output_paths = recomputed.output_paths;
        self.gql_query_type = recomputed.gql_query_type;
        self.output_table = recomputed.output_table;
        self.dispatch_table = recomputed.dispatch_table;
        self.unified_type_doc = recomputed.unified_type_doc;
        self.node_to_output_tables = recomputed.node_to_output_tables;
        Ok(())
    }

    /// Merge a file into the current as a mutation. Returns a list of _updated_ nodes by name.
    pub fn merge_file(&mut self, file: &File) -> anyhow::Result<Vec<String>> {
        // We merge each node in the file into our own file, combining their keys
        let mut updated_nodes = vec![];
        for node in file.nodes.iter() {
            if node.item.is_none() { continue; }

            // Nodes with the same name are merged
            let name = &node.core.as_ref().unwrap().name;

            // If the node does not exist we just insert it
            if !self.node_by_name.contains_key(name) {
                self.node_by_name.insert(name.clone(), node.clone());
            } else {
                updated_nodes.push(name.clone());
                // Otherwise we need to merge the node field by field
                let existing = self.node_by_name.get_mut(name).unwrap();

                let core = node.core.clone().unwrap();
                if let Some(existing_core) = &mut existing.core {
                    existing_core.name = core.name;
                    existing_core.output = core.output;
                    existing_core.queries = core.queries;
                }

                match node.item.clone().unwrap() {
                    dsl_item::Item::NodeParameter(_n) => {
                        if let Some(dsl_item::Item::NodeParameter(_ex)) = &mut existing.item {
                        }
                    },
                    dsl_item::Item::Map(n) => {
                        if let Some(dsl_item::Item::Map(ex)) = &mut existing.item {
                            ex.path = n.path;
                        }
                    },
                    dsl_item::Item::NodeCode(n) => {
                        if let Some(dsl_item::Item::NodeCode(ex)) = &mut existing.item {
                            ex.source = n.source;
                        }
                    },
                    dsl_item::Item::NodePrompt(n) => {
                        if let Some(dsl_item::Item::NodePrompt(ex)) = &mut existing.item {
                            ex.template = n.template;
                            ex.model = n.model;
                            ex.temperature = n.temperature;
                            ex.top_p = n.top_p;
                            ex.max_tokens = n.max_tokens;
                            ex.presence_penalty = n.presence_penalty;
                            ex.frequency_penalty = n.frequency_penalty;
                            ex.stop = n.stop;
                        }
                    },
                    dsl_item::Item::NodeMemory(n) => {
                        if let Some(dsl_item::Item::NodeMemory(ex)) = &mut existing.item {
                            ex.template = n.template;
                            ex.embedding_model = n.embedding_model;
                            ex.vector_db_provider = n.vector_db_provider;
                            ex.action = n.action;
                        }
                    },
                    dsl_item::Item::NodeComponent(n) => {
                        if let Some(dsl_item::Item::NodeComponent(ex)) = &mut existing.item {
                            ex.transclusion = n.transclusion
                        }
                    },
                    dsl_item::Item::NodeObservation(n) => {
                        if let Some(dsl_item::Item::NodeObservation(ex)) = &mut existing.item {
                            ex.integration = n.integration;
                        }
                    },
                    dsl_item::Item::NodeEcho(_n) => {
                        if let Some(dsl_item::Item::NodeEcho(_ex)) = &mut existing.item {
                        }
                    },
                    dsl_item::Item::NodeLoader(n) => {
                        if let Some(dsl_item::Item::NodeLoader(ex)) = &mut existing.item {
                            ex.load_from = n.load_from;
                        }
                    },
                    dsl_item::Item::NodeCustom(n) => {
                        if let Some(dsl_item::Item::NodeCustom(ex)) = &mut existing.item {
                            ex.type_name = n.type_name;
                        }
                    },
                    _ => return Err(anyhow!("Node type not supported"))
                };
            }
        }

        let recomputed = CleanedDefinitionGraph::recompute_parsed_values(
            mem::take(&mut self.node_by_name)
        ).unwrap();

        // TODO: Validate the new query that was added, if any
        // for node in file.nodes.iter() {
        //     for opt_query_doc in recomputed.query_types.get(&node.core.as_ref().unwrap().name).unwrap() {
        //         if let Some(query_doc) = opt_query_doc {
        //             validate_new_query(&recomputed.unified_type_doc, query_doc);
        //         }
        //     }
        // }

        self.node_by_name = recomputed.node_by_name;
        self.query_types = recomputed.query_types;
        self.query_paths = recomputed.query_paths;
        self.output_types = recomputed.output_types;
        self.output_paths = recomputed.output_paths;
        self.gql_query_type = recomputed.gql_query_type;
        self.output_table = recomputed.output_table;
        self.dispatch_table = recomputed.dispatch_table;
        self.unified_type_doc = recomputed.unified_type_doc;
        self.node_to_output_tables = recomputed.node_to_output_tables;

        Ok(updated_nodes)
    }

    /// Hashjoin output and dispatch tables
    fn join_relation_between_output_and_dispatch_tables(&self) -> Vec<(String, String)> {
        let mut edges: Vec<(String, String)> = vec![];
        for (key, originating_nodes) in self.output_table.iter() {
            for originating_node in originating_nodes {
                if let Some(affecting_nodes) = self.dispatch_table.get(key) {
                    for affecting_node in affecting_nodes {
                        edges.push((originating_node.clone(), affecting_node.clone()));
                    }
                }
            }
        }
        edges
    }

    pub fn get_dot_graph(&self) -> String {
        let mut graph: DiGraphMap<u32, u32> = petgraph::graphmap::DiGraphMap::new();

        // Convert nodes into a numeric representation
        let mut nodes = HashMap::new();
        let mut nodes_inverse = HashMap::new();
        let mut counter: u32 = 0;
        let mut keys: Vec<&String> = self.node_by_name.keys().collect();
        keys.sort();
        for node_name in keys {
            nodes.insert(node_name, counter);
            nodes_inverse.insert(counter, node_name);
            graph.add_node(counter);
            counter += 1;
        }

        // Join output and dispatch tables
        let mut edges = self.join_relation_between_output_and_dispatch_tables();
        edges.sort();
        for (originating_node, affecting_node) in edges {
            graph.add_edge(*nodes.get(&originating_node).unwrap(), *nodes.get(&affecting_node).unwrap(), 0);
        }

        // TODO: this shows an error in intellij but it compiles fine
        format!("{:?}", Dot::with_attr_getters(
            &graph,
            &[Config::EdgeNoLabel],
            &|_, _| { "".to_string() },
            &|_, (n, _w)| {
                format!("label=\"{}\"", nodes_inverse.get(&n).unwrap())
            }
        ))
    }

}

fn validate_new_query(unified_type_doc: &Document, new_query: &Document) -> anyhow::Result<()> {
    let mut compiler = ApolloCompiler::new();
    compiler.add_type_system(unified_type_doc.to_string().as_str(), "".to_string());
    validate_query_type(&mut compiler, "Query".to_string(), new_query);
    Ok(())
}

pub fn build_type_document(docs: Vec<(&String, &EncoderDocument)>, query_operation_defintion: &EncoderDocument) -> EncoderDocument {
    let mut unified_doc = EncoderDocument::new();
    unified_doc.object_type_definitions.extend(query_operation_defintion.object_type_definitions.clone());
    for (_name, doc) in docs {
        unified_doc.object_type_definitions.extend(doc.object_type_definitions.clone());
    }
    unified_doc
}

pub fn validate_query_type(apollo_compiler: &mut ApolloCompiler, name: String, query: &Document) -> Vec<String> {
    apollo_compiler.add_executable(&query.to_string(), name);
    let diagnostics = apollo_compiler.validate();
    diagnostics.into_iter().map(|d| d.to_string()).collect()
}


fn add_to_field(
    keys: &Vec<String>,
    selection_set: &mut SelectionSet
) -> anyhow::Result<()> {
    if keys.len() == 0 {
        return Ok(())
    }
    let key = &keys[0];
    if let Some(Selection::Field(existing_field)) = selection_set.selections.iter_mut().find(|x| {
        if let Selection::Field(f) = x {
            &f.name == key
        } else {
            false
        }
    }) {
        // Continue to traverse to deeper levels of the field, no mutation
        add_to_field(&keys[1..].to_vec(), existing_field.selection_set.as_mut().unwrap())?;
    } else {
        // Add a new field for this key to the selection
        selection_set.selections.push(Selection::Field(EncoderField::new(key.clone())));
        let last_elem = selection_set.selections.last_mut().unwrap();
        if let Selection::Field(f) = last_elem {
            let next_keys = &keys[1..].to_vec();
            if next_keys.len() == 0 {
                return Ok(())
            }
            if f.selection_set.is_none() {
                f.selection_set = Some(SelectionSet::new());
            }
            add_to_field( &next_keys, f.selection_set.as_mut().unwrap())?;
        } else {
            anyhow::bail!("Expected field");
        }
    }
    Ok(())
}

pub fn construct_query_from_output_type(name: &String, namespace: &String, output_paths: &OutputPath) -> anyhow::Result<String> {
    let mut encoder = EncoderDocument::new();

    // TODO: this needs to start empty
    let mut selection_set = SelectionSet::new();
    for output_path in output_paths {
        add_to_field(&output_path, &mut selection_set).unwrap();
    }

    encoder.operation( apollo_encoder::OperationDefinition {
            operation_type: apollo_encoder::OperationType::Query,
            name: Some(utils::uppercase_first_letter(&name)),
            variable_definitions: vec![],
            directives: vec![],
            selection_set,
            shorthand: false,
        }
    );
    Ok(encoder.to_string())
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::graph_definition::{create_code_node, SourceNodeType};
    use crate::proto2::Query;

    use super::*;

    fn gen_item_hello(output_tables: Vec<String>) -> Item {
        create_code_node(
            "code_node_test".to_string(),
            vec![None],
            r#" type O { output: String }"#.to_string(),
            SourceNodeType::Code(String::from("DENO"),
                                 indoc! { r#"
                            return {
                                "output": "hello"
                            }
                        "#}.to_string(),
                                 false
            ),
            output_tables
        )
    }

    fn gen_item_hello_plus_world() -> Item {
        create_code_node(
            "code_node_test_dep".to_string(),
            vec![Some( r#" query Q {
                code_node_test {
                    output
                }
            }"#.to_string(),
            )],
            r#"type O { result: String }"#.to_string(),
            SourceNodeType::Code(String::from("DENO"),
                                 indoc! { r#"
                            return {
                                "result": "{{code_node_test.output}}" + " world"
                            }
                        "#}.to_string(),
                                 true
            ),
            vec![]
        )
    }

    #[test]
    fn test_get_paths_for_query() {
        let enc = parse_graphql_query_def("query", "query Name { user { id, name } }").unwrap();
        let paths = get_paths_for_query(&enc);
        assert_eq!(paths,
                   vec![vec!["user".to_string(), "id".to_string()], vec!["user".to_string(), "name".to_string()]]);
    }

    #[test]
    fn test_get_paths_for_output() {
        let enc = parse_graphql_type_def("query", "type RandomDie { numSides: Int! rollOnce: Int! } ").unwrap();
        let paths = get_paths_for_output(&HashSet::from(["node".to_string()]), &enc);
        assert_eq!(paths,
                   vec![vec!["node".to_string(), "numSides".to_string()], vec!["node".to_string(), "rollOnce".to_string()]]);
    }

    #[test]
    fn test_extract_query_types() {
        let item_core = ItemCore {
            name: "Name".to_string(),
            queries: vec![Query {
                query: Some("query Q { user { id, name }}".to_string())
            }],
            output_tables: vec![],
            output: None,
        };
        let docs = extract_query_types(&item_core).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].is_some(), true);
        let paths = get_paths_for_query(&docs[0].as_ref().unwrap());
        assert_eq!(paths, vec![vec!["user".to_string(), "id".to_string()], vec!["user".to_string(), "name".to_string()]]);
    }

    #[test]
    fn test_extract_query_types_multiple_queries_and_one_none() {
        let item_core = ItemCore {
            name: "Name".to_string(),
            queries: vec![
                Query { query: Some("query Q { user { id, name } }".to_string()) },
                Query { query: Some("query Q { account { uuid, fullName } }".to_string()) },
                Query { query: None },
            ],
            output_tables: vec![],
            output: None,
        };
        let docs = extract_query_types(&item_core).unwrap();
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].is_some(), true);
        let paths = get_paths_for_query(&docs[0].as_ref().unwrap());
        assert_eq!(paths, vec![vec!["user".to_string(), "id".to_string()], vec!["user".to_string(), "name".to_string()]]);

        assert_eq!(docs[1].is_some(), true);
        let paths = get_paths_for_query(&docs[1].as_ref().unwrap());
        assert_eq!(paths, vec![vec!["account".to_string(), "uuid".to_string()], vec!["account".to_string(), "fullName".to_string()]]);

        assert_eq!(docs[2].is_none(), true);
    }


    #[test]
    fn test_building_a_graphql_query_from_output_paths() {
        let r = construct_query_from_output_type(
            &"Name".to_string(),
            &"Namespace".to_string(),
            &vec![vec!["user".to_string(), "id".to_string()], vec!["user".to_string(), "name".to_string()]]).unwrap();
        assert_eq!(r, indoc! { r#"
            query Name {
              user {
                id
                name
              }
            }
        "#});
    }

    #[test]
    fn test_producing_valid_dot_graph() {
        let mut file = File {
            id: "test".to_string(),
            nodes: vec![
                gen_item_hello(vec![]),
                // Our goal is to see changes from this node on both branches
                gen_item_hello_plus_world()
            ],
        };
        let mut g = CleanedDefinitionGraph::zero();
        g.merge_file(&mut file).unwrap();

        assert_eq!(g.join_relation_between_output_and_dispatch_tables(), vec![("code_node_test".to_string(), "code_node_test_dep".to_string())]);

        assert_eq!(g.get_dot_graph(), indoc!{r#"
            digraph {
                0 [ label = "0" label="code_node_test"]
                1 [ label = "1" label="code_node_test_dep"]
                0 -> 1 [ ]
            }
        "#});
    }


    #[test]
    fn test_producing_valid_dot_graph_with_output_table() {
        env_logger::init();
        let mut file = File {
            id: "test".to_string(),
            nodes: vec![
                gen_item_hello(vec!["OutputTable2".to_string()]),
                // Our goal is to see changes from this node on both branches
                gen_item_hello_plus_world(),
                create_code_node(
                    "code_node_test_dep_output".to_string(),
                    vec![Some( r#" query Q {
                        OutputTable2 {
                            output
                        }
                    }"#.to_string(),
                    )],
                    r#"type O { result: String }"#.to_string(),
                    SourceNodeType::Code(String::from("DENO"),
                                         indoc! { r#"
                            return {
                                "result": "{{code_node_test.output}}" + " world"
                            }
                        "#}.to_string(),
                                         true
                    ),
                    vec![]
                )
            ],
        };
        let mut g = CleanedDefinitionGraph::zero();
        g.merge_file(&mut file).unwrap();

        let mut list = g.join_relation_between_output_and_dispatch_tables();
        list.sort();
        assert_eq!(list, vec![
            ("code_node_test".to_string(), "code_node_test_dep".to_string()),
            ("code_node_test".to_string(), "code_node_test_dep_output".to_string()),
        ]);

        assert_eq!(g.get_dot_graph(), indoc!{r#"
            digraph {
                0 [ label = "0" label="code_node_test"]
                1 [ label = "1" label="code_node_test_dep"]
                2 [ label = "2" label="code_node_test_dep_output"]
                0 -> 1 [ ]
                0 -> 2 [ ]
            }
        "#});
    }

}
