use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::{fmt, mem};

use anyhow::anyhow;
use gluesql::core::ast::ToSql;
use gluesql::core::ast_builder::{Build, col, Execute, table};
use indoc::indoc;
use petgraph::dot::{Config, Dot};
use petgraph::graphmap::DiGraphMap;
use serde::de;
use sqlparser::ast::{Expr, JoinConstraint, Query, Select, SelectItem, SetExpr, Statement, TableWithJoins};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::{Parser, ParserError};

use crate::graph_definition::DefinitionGraph;
use crate::proto2::{File, Item, item as dsl_item, ItemCore, OutputType};
use crate::utils;
use crate::utils::wasm_error::CoreError;


// Used for typing outputs
pub enum SQLType {
    Number,
    Text,
    Timestamp,
    Boolean,
    Null,
}

impl SQLType {
    pub fn from_str(s: &str) -> anyhow::Result<SQLType> {
        let s = s.to_lowercase();
        match s.as_str() {
            "integer" => Ok(SQLType::Number),
            "float" => Ok(SQLType::Number),
            "string" => Ok(SQLType::Text),
            "text" => Ok(SQLType::Text),
            "date" => Ok(SQLType::Timestamp),
            "timestamp" => Ok(SQLType::Timestamp),
            "boolean" => Ok(SQLType::Boolean),
            "bool" => Ok(SQLType::Boolean),
            "null" => Ok(SQLType::Null),
            _ => Err(anyhow!("Unknown SQL type {}", s)),
        }
    }
}

impl<'de> serde::Deserialize<'de> for SQLType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
    {
        struct SQLTypeVisitor;

        impl<'de> serde::de::Visitor<'de> for SQLTypeVisitor {
            type Value = SQLType;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a valid string for SQLType")
            }

            fn visit_str<E>(self, value: &str) -> Result<SQLType, E>
                where
                    E: serde::de::Error,
            {
                SQLType::from_str(value).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(SQLTypeVisitor)
    }
}

pub type OutputTypeDefinition = HashMap<String, SQLType>;

pub fn parse_output_type_def_to_paths(input: &str) -> OutputPaths {
    serde_json::from_str(input).unwrap()
}

pub fn parse_projection_values(input: &str) -> Vec<String> {
    let dialect = GenericDialect {};
    let ast = Parser::parse_sql(&dialect, input).unwrap();
    let mut projected_values = Vec::new();
    for stmt in ast {
        if let Statement::Query(query) = stmt {
            if let SetExpr::Select(select) = *query.body {
                for projection in select.projection {
                    if let SelectItem::UnnamedExpr(Expr::Identifier(identifier)) = projection {
                        projected_values.push(identifier.value);
                    }
                }
            }
        }
    }
    projected_values
}


// TODO: using a parsed output type, generate a maximal operation query definition
fn generate_maximal_operation_def_from_output() {
    unimplemented!();
}


use sqlparser::ast::{TableFactor, ObjectName, Join, JoinOperator};

fn parse_expr(expr: &Expr, tables_and_columns: &mut Vec<(String, Vec<String>)>) {
    match expr {
        Expr::CompoundIdentifier(identifier) => {
            if identifier.len() == 2 {
                let table_name = identifier[0].value.to_string();
                let column_name = identifier[1].value.to_string();
                if let Some((_, columns)) = tables_and_columns.iter_mut().find(|(table, _)| table == &table_name) {
                    columns.push(column_name);
                }
            }
        },
        Expr::Identifier(ident) => {
            let column_name = ident.value.to_string();
            for (_, columns) in tables_and_columns {
                columns.push(column_name.clone());
            }
        },
        Expr::BinaryOp { left, right, .. } => {
            parse_expr(&*left, tables_and_columns);
            parse_expr(&*right, tables_and_columns);
        }
        _ => {}
    }
}

pub fn parse_tables_and_columns(input: &str) -> Result<Vec<(String, Vec<String>)>, sqlparser::parser::ParserError> {
    let dialect = GenericDialect {};
    let ast: Result<Vec<Statement>, ParserError> = Parser::parse_sql(&dialect, input);

    let mut tables_and_columns = Vec::new();

    match ast {
        Ok(parsed) => {
            for stmt in parsed {
                if let Statement::Query(query) = stmt {
                    let query: Query = *query;
                    if let SetExpr::Select(select) = *query.body {
                        let select: Select = *select;
                        for from in &select.from {
                            let from: &TableWithJoins = from;
                            // handle direct table
                            if let TableFactor::Table { name, alias, args: _, with_hints: _ } = &from.relation {
                                let table_name = match alias {
                                    Some(alias) => alias.name.value.to_string(),
                                    None => name.to_string(),
                                };
                                tables_and_columns.push((table_name, vec![]));
                            }

                            // handle joins
                            for join in &from.joins {
                                let join: &Join = join;
                                if let TableFactor::Table { name, alias, args: _, with_hints: _ } = &join.relation {
                                    let table_name = match alias {
                                        Some(alias) => alias.name.value.to_string(),
                                        None => name.to_string(),
                                    };
                                    tables_and_columns.push((table_name, vec![]));
                                }
                                // Handle "ON" clause in joins
                                if let Some(constraint) = match &join.join_operator {
                                    JoinOperator::Inner(c) => Some(c),
                                    JoinOperator::LeftOuter(c) => Some(c),
                                    JoinOperator::RightOuter(c) => Some(c),
                                    JoinOperator::FullOuter(c) => Some(c),
                                    JoinOperator::CrossJoin => None,
                                    JoinOperator::LeftSemi(c) => Some(c),
                                    JoinOperator::RightSemi(c) => Some(c),
                                    JoinOperator::LeftAnti(c) => Some(c),
                                    JoinOperator::RightAnti(c) => Some(c),
                                    JoinOperator::CrossApply => None,
                                    JoinOperator::OuterApply => None,
                                } {
                                    if let JoinConstraint::On(expr) = &constraint {
                                        parse_expr(expr, &mut tables_and_columns);
                                    }
                                }
                            }
                        }


                        for projection in &select.projection {
                            if let SelectItem::Wildcard(opts) = projection {
                                for (_, columns) in &mut tables_and_columns {
                                    columns.push("*".to_string());
                                }
                            } else if let SelectItem::ExprWithAlias { expr, .. } = projection {
                                parse_expr(&expr, &mut tables_and_columns);
                            } else if let SelectItem::UnnamedExpr(expr) = projection {
                                parse_expr(&expr, &mut tables_and_columns);
                            }
                        }

                        if let Some(where_clause) = &select.selection {
                            parse_expr(where_clause, &mut tables_and_columns);
                        }
                    }
                }
            }
            Ok(tables_and_columns)
        },
        Err(e) => Err(e),
    }
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


#[derive(Debug, Clone)]
pub struct CleanIndividualNode {
    pub name: String,
    pub query_path: QueryPath,
    pub output_paths: OutputPaths,
    pub output_tables: HashSet<String>,
}

pub fn derive_for_individual_node(node: &Item) -> anyhow::Result<CleanIndividualNode> {
    let name = &node.core.as_ref().unwrap().name;
    let core = node.core.as_ref().unwrap();

    let mut query_path: QueryPath = vec![];
    for query in &core.queries {
        if let Some(q) = &query.query {
            let paths = query_path_from_query_string(&q)?;
            query_path.push(Some(paths));
        } else {
            query_path.push(None);
        }
    }

    // Add the node to the output table


    let mut output_tables: HashSet<_> = core.output_tables.iter().cloned().collect();
    output_tables.insert(core.name.clone());

    let mut output_paths = vec![];
    for output in &core.output {
        let output_type: OutputTypeDefinition = serde_yaml::from_str(&output.output)?;
        for output_table in &output_tables {
            for (output_key, _ty) in &output_type {
                output_paths.push(vec![output_table.clone(), output_key.clone()]);
            }
        }
    }

    Ok(CleanIndividualNode {
        name: name.clone(),
        query_path,
        output_paths,
        output_tables
    })
}

pub fn query_path_from_query_string(q: &String) -> anyhow::Result<Vec<Vec<String>>> {
    let dependent_on = parse_tables_and_columns(&q)?;
    let mut paths = vec![];
    for table in dependent_on {
        let mut path_segment = vec![table.0];
        path_segment.extend(table.1);
        paths.push(path_segment);
    }
    Ok(paths)
}


type QueryVecGroup = Vec<Vec<String>>;
type QueryPath =  Vec<Option<QueryVecGroup>>;
type OutputPaths =  Vec<Vec<String>>;


#[derive(Debug, Clone)]
pub struct CleanedDefinitionGraph {
    pub query_paths: HashMap<String, QueryPath>,
    pub node_by_name: HashMap<String, Item>,
    pub dispatch_table: HashMap<String, Vec<String>>,
    pub output_table: HashMap<String, Vec<String>>,
    pub node_to_output_tables: HashMap<String, HashSet<String>>,
    pub output_paths: HashMap<String, OutputPaths>,

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
            query_paths: HashMap::new(),
            output_table: HashMap::new(),
            dispatch_table: HashMap::new(),
            output_paths: HashMap::new(),
            node_by_name: HashMap::new(),
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
            graph.node_to_output_tables.insert(name.clone(), mem::take(&mut indiv.output_tables));
            // graph.query_types.insert(name.clone(), mem::take(&mut indiv.query_type));
            // graph.output_types.insert(name.clone(), mem::take(&mut indiv.output_type));
            graph.query_paths.insert(name.clone(), mem::take(&mut indiv.query_path));
            graph.output_paths.insert(name.clone(), mem::take(&mut indiv.output_paths));
        }

        // This aggregates all the query types into a single query type
        // graph.gql_query_type = generate_gql_schema_query_type(&graph.output_types);
        graph.output_table = output_table_from_output_types(&graph.output_paths);
        graph.dispatch_table = dispatch_table_from_query_paths(&graph.query_paths);
        // graph.unified_type_doc = build_type_document(graph.output_types.iter().collect(), &graph.gql_query_type);
        graph.node_by_name = node_by_name;

        Ok(graph)
    }

    pub fn assert_parsing(&mut self) -> anyhow::Result<()> {
        let recomputed = CleanedDefinitionGraph::recompute_parsed_values(
            mem::take(&mut self.node_by_name)
        ).unwrap();
        self.node_by_name = recomputed.node_by_name;
        self.query_paths = recomputed.query_paths;
        self.output_paths = recomputed.output_paths;
        self.output_table = recomputed.output_table;
        self.dispatch_table = recomputed.dispatch_table;
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
        self.query_paths = recomputed.query_paths;
        self.output_paths = recomputed.output_paths;
        self.output_table = recomputed.output_table;
        self.dispatch_table = recomputed.dispatch_table;
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




pub fn construct_query_from_output_type(name: &String, namespace: &String, output_paths: &OutputPaths) -> anyhow::Result<String> {
    let projection_items: Vec<String> = output_paths.iter().map(|x| format!("{}.{}", name, x.join("."))).collect();
    let projection = projection_items.join(", ");
    Ok(format!("SELECT {} FROM {}", projection, namespace))
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
            r#"{ "output": String }"#.to_string(),
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
            vec![Some( r#" SELECT output FROM code_node_test"#.to_string(),
            )],
            r#"{ "result": String }"#.to_string(),
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
    fn test_construct_query_from_output_type() {
        let output_paths : OutputPaths = vec![vec!["output".to_string()]];
        let query = construct_query_from_output_type(&"code_node_test".to_string(), &"code_node_test".to_string(), &output_paths).unwrap();
        assert_eq!(query, "SELECT code_node_test.output FROM code_node_test");
    }

    #[test]
    fn test_construct_query_from_output_type_multiple_keys() {
        let output_paths : OutputPaths = vec![vec!["output".to_string()], vec!["result".to_string()]];
        let query = construct_query_from_output_type(&"code_node_test".to_string(), &"code_node_test".to_string(), &output_paths).unwrap();
        assert_eq!(query, "SELECT code_node_test.output, code_node_test.result FROM code_node_test");
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
                    vec![Some( r#"SELECT output FROM OutputTable2"#.to_string(),
                    )],
                    r#"{ result: String }"#.to_string(),
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

    #[test]
    fn test_parse_projection_values() {
        let sql_query = "SELECT column1, column2 FROM table_1 WHERE column1 = 'value'";
        let result = parse_projection_values(sql_query);
        assert_eq!(result, vec!["column1", "column2"]);

        let sql_query = "SELECT column1, column2, column3 FROM table_1 WHERE column1 = 'value' AND column2 = 'value2'";
        let result = parse_projection_values(sql_query);
        assert_eq!(result, vec!["column1", "column2", "column3"]);
    }

    // Extracting the tables and associated columns used in the sql query

    #[test]
    fn test_single_table_no_alias() {
        let sql = "SELECT col1, col2 FROM table1";
        let result = parse_tables_and_columns(sql);
        assert_eq!(result.unwrap(), vec![("table1".to_string(), vec!["col1".to_string(), "col2".to_string()])]);
    }

    #[test]
    fn test_single_table_with_alias() {
        let sql = "SELECT t.col1, t.col2 FROM table1 AS t";
        let result = parse_tables_and_columns(sql);
        assert_eq!(result.unwrap(), vec![("t".to_string(), vec!["col1".to_string(), "col2".to_string()])]);
    }

    #[test]
    fn test_joined_tables_no_alias() {
        let sql = "SELECT table1.col1, table2.col2 FROM table1 JOIN table2 ON table1.id = table2.id";
        let result = parse_tables_and_columns(sql);
        let expected = vec![
            ("table1".to_string(), vec!["id".to_string(), "col1".to_string()]),
            ("table2".to_string(), vec!["id".to_string(), "col2".to_string()]),
        ];
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn test_joined_tables_with_alias() {
        let sql = "SELECT t1.col1, t2.col2 FROM table1 AS t1 JOIN table2 AS t2 ON t1.id = t2.id";
        let result = parse_tables_and_columns(sql);
        let expected = vec![
            ("t1".to_string(), vec!["id".to_string(), "col1".to_string()]),
            ("t2".to_string(), vec!["id".to_string(), "col2".to_string()]),
        ];
        assert_eq!(result.unwrap(), expected);
    }

}
