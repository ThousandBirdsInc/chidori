/// This constructs a set of triggerable relationships via a group of SQL statements. These statements
/// may include conditional logic, and may be parameterized by the values of other triggerable relationships.
/// When the result of statement execution changes, the associated triggerable relationship is fired.
/// This is built on top of the triggerable abstraction defined in `triggerable.rs`.

use std::fmt;
use anyhow::anyhow;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::ast::{Expr, Join, JoinConstraint, JoinOperator, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins};
use serde::de::Error;
use serde::Deserializer;

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

pub fn parse_expr(expr: &Expr, tables_and_columns: &mut Vec<(String, Vec<String>)>) {
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
                            if let SelectItem::Wildcard(_opts) = projection {
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



#[cfg(test)]
mod tests {
    use indoc::indoc;
    use super::*;


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
