//! Implementation of "Complete and Easy Bidirectional Typechecking for Higher-Rank Polymorphism"
//! See: https://github.com/JDemler/BidirectionalTypechecking/blob/master/src/main.rs
//! See: https://arxiv.org/abs/1306.6032
//!
//! The main focus of this implementation lies beeing able to follow the paper while reading it
//! I tried to keep naming consistent and referencing where things are defined in the paper
//! No sensible error reporting is implemented. Failures will simply result in panics
//!
//! This is an extended version. Check out original.rs for the original implementation.

use std::fmt;

///Figure 6
/// The `Expression` enum represents the different types of expressions that can be parsed and evaluated.
/// Each variant of the enum corresponds to a different kind of expression.
/// - `Variable`: A string that represents a variable name.
/// - `Literal`: A literal value, such as a number or string.
/// - `Abstraction`: A function abstraction, which includes a string for the function name and a box containing another expression for the function body.
/// - `Application`: A function application, which includes a box containing the function to be applied and another box containing the expression to apply the function to.
/// - `Let`: A let expression, which includes a string for the variable name, a box containing the expression to assign to the variable, and another box containing the expression in which to use the variable.
/// - `Annotation`: An expression with a type annotation, which includes a box containing the expression and the type to annotate the expression with.
/// - `Tuple`: A tuple of two expressions, each contained in a box.
#[derive(Clone, Debug)]
enum Expression {
    Variable(String),
    Literal(Literal),
    Abstraction(String, Box<Expression>),
    Application(Box<Expression>, Box<Expression>),
    Let(String, Box<Expression>, Box<Expression>),
    Annotation(Box<Expression>, Type),
    Tuple(Box<Expression>, Box<Expression>),
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            Expression::Literal(lit) => write!(f, "{}", lit),
            Expression::Variable(var) => write!(f, "{}", var),
            Expression::Abstraction(alpha, e) => write!(f, "(\\{} -> {})", alpha, e),
            Expression::Application(e1, e2) => write!(f, "{} {}", e1, e2),
            Expression::Let(var, expr, body) => write!(f, "let {} = {} in {}", var, expr, body),
            Expression::Annotation(e, a) => write!(f, "({}: {})", e, a),
            Expression::Tuple(fst, snd) => write!(f, "({}, {})", fst, snd),
        }
    }
}

/// The `Literal` enum represents the different types of literal values that can be parsed and evaluated.
/// Each variant of the enum corresponds to a different kind of literal.
/// - `Char`: A character literal.
/// - `String`: A string literal.
/// - `Int`: An integer literal.
/// - `Float`: A floating point number literal.
/// - `Bool`: A boolean literal.
/// - `Unit`: The unit type, representing an empty tuple.
#[derive(Clone, Debug)]
enum Literal {
    Char(char),
    String(String),
    Int(isize),
    Float(f64),
    Bool(bool),
    Unit,
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            Literal::Char(val) => write!(f, "'{}'", val),
            Literal::String(val) => write!(f, "'{}'", val),
            Literal::Int(val) => write!(f, "{}", val),
            Literal::Float(val) => write!(f, "{}", val),
            Literal::Bool(val) => write!(f, "{}", val),
            Literal::Unit => write!(f, "()"),
        }
    }
}

///Figure 6
#[derive(Clone, Debug, PartialEq, Eq)]
enum Type {
    Literal(LiteralType),
    Variable(String),
    Existential(String),
    Quantification(String, Box<Type>),
    Function(Box<Type>, Box<Type>),
    Product(Box<Type>, Box<Type>),
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            Type::Literal(lit) => write!(f, "{}", lit),
            Type::Variable(var) => write!(f, "{}", var),
            Type::Existential(ex) => write!(f, "{}^", ex),
            Type::Quantification(a, ty) => write!(f, "(∀{}. {})", a, ty),
            Type::Function(a, c) => write!(f, "({} -> {})", a, c),
            Type::Product(a, b) => write!(f, "{} × {}", a, b),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LiteralType {
    Unit,
    Char,
    String,
    Int,
    Float,
    Bool,
}

impl fmt::Display for LiteralType {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            LiteralType::Unit => write!(f, "()"),
            LiteralType::Char => write!(f, "Char"),
            LiteralType::String => write!(f, "String"),
            LiteralType::Int => write!(f, "Int"),
            LiteralType::Float => write!(f, "Float"),
            LiteralType::Bool => write!(f, "Bool"),
        }
    }
}

impl Type {
    fn is_monotype(&self) -> bool {
        match self {
            Type::Quantification(..) => false,
            Type::Function(t1, t2) => t1.is_monotype() && t2.is_monotype(),
            _ => true,
        }
    }
}

/// The `ContextElement` enum represents the different types of elements that can be present in a context.
/// Each variant of the enum corresponds to a different kind of element.
/// - `Variable`: A string that represents a variable name.
/// - `Existential`: A string that represents an existential variable.
/// - `Solved`: A tuple containing a string for the variable name and a `Type` for the type it has been solved to.
/// - `Marker`: A string that represents a marker variable.
/// - `TypedVariable`: A tuple containing a string for the variable name and a `Type` for its type.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContextElement {
    Variable(String),
    Existential(String),
    Solved(String, Type),
    Marker(String),
    TypedVariable(String, Type),
}

impl fmt::Display for ContextElement {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            ContextElement::Variable(var) => write!(f, "{}", var),
            ContextElement::Existential(ex) => write!(f, "{}^", ex),
            ContextElement::Solved(a, ty) => write!(f, "{}^: {}", a, ty),
            ContextElement::Marker(a) => write!(f, "<|{}", a),
            ContextElement::TypedVariable(x, ty) => write!(f, "{}: {}", x, ty),
        }
    }
}

/// As the context needs to be ordered, it is implemented as a simple Vector.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Context {
    elements: Vec<ContextElement>,
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "[").unwrap();
        &self.elements.iter().fold(true, |first, ele| {
            if !first {
                write!(f, ", ").unwrap()
            };
            write!(f, "{}", ele).unwrap();
            false
        });
        write!(f, "]")
    }
}

/// Context operations derive from "Hole notation" described in 3.1 and the fact that the context is ordered.
/// The context is manipulated not only by appending declarations but also by inserting
/// and replacing declarations in the middle. This is represented by the "Hole notation":
/// Γ = Γ0[Θ] means Γ has the form (ΓL, Θ, ΓR)
/// For example, if Γ = Γ0[β^] = (^α, β, x ^ : β^), then Γ0[β^ = ^α] = (^α, β^ = ^α, x : β^).
/// Occasionally, we also need contexts with two ordered holes:
/// Γ = Γ0[Θ1][Θ2] means Γ has the form (ΓL, Θ1, ΓM, Θ2, ΓR)
/// The context is used as an input and evolves into an output context that is “more solved”.
impl Context {
    /// Creates a new, empty context.
    fn initial() -> Self {
        Context {
            elements: Vec::new(),
        }
    }

    /// Adds a new element to the context.
    fn add(&self, element: ContextElement) -> Self {
        let mut eles = self.elements.clone();
        eles.push(element);
        Context { elements: eles }
    }

    /// Splits the context at a given element, returning two new contexts.
    /// This is used when we need contexts with a hole in the middle.
    fn split_at(&self, element: ContextElement) -> (Context, Context) {
        if let Some(index) = self.elements.iter().position(|ele| ele == &element) {
            let (lhs, rhs) = self.elements.split_at(index);
            let left_context = Context {
                elements: lhs.to_vec(),
            };
            let right_context = Context {
                elements: rhs.to_vec(),
            };

            return (left_context, right_context);
        }
        panic!();
    }

    /// Inserts a vector of elements in place of a given element in the context.
    /// This is used when we need to replace a declaration in the context.
    fn insert_in_place(&self, element: ContextElement, inserts: Vec<ContextElement>) -> Self {
        if let Some(index) = self.elements.iter().position(|ele| ele == &element) {
            let mut eles = self.elements.clone();
            let _ = eles.splice(index..=index, inserts).count();
            return Context { elements: eles };
        }
        panic!();
    }

    /// Drops a given element from the context.
    /// This is used when we need to remove a declaration from the context.
    fn drop(&self, element: ContextElement) -> Self {
        if let Some(index) = self.elements.iter().position(|ele| ele == &element) {
            let mut eles = self.elements.clone();
            eles.split_off(index);
            return Context { elements: eles };
        }
        panic!();
    }

    /// Returns the type of a solved variable, if it exists in the context.
    /// This is used when we need to find the type that an existential variable has been solved to.
    fn get_solved(&self, alpha: &str) -> Option<&Type> {
        for ele in &self.elements {
            if let ContextElement::Solved(alpha1, tau) = ele {
                if alpha == alpha1 {
                    return Some(tau);
                }
            }
        }
        None
    }

    /// Checks if an existential variable exists in the context.
    /// This is used when we need to check if an existential variable is declared in the context.
    fn has_existential(&self, alpha: &str) -> bool {
        self.elements
            .iter()
            .any(|ele| ele == &ContextElement::Existential(alpha.to_string()))
    }

    /// Checks if a variable exists in the context.
    /// This is used when we need to check if a variable is declared in the context.
    fn has_variable(&self, alpha: &str) -> bool {
        self.elements
            .iter()
            .any(|ele| ele == &ContextElement::Variable(alpha.to_string()))
    }

    /// Returns the type annotation of a variable, if it exists in the context.
    /// This is used when we need to find the type that a variable is annotated with.
    fn get_annotation(&self, x: &str) -> Option<&Type> {
        for ele in &self.elements {
            if let ContextElement::TypedVariable(var, type_) = ele {
                if var == x {
                    return Some(type_);
                }
            }
        }
        None
    }
}

/// The `State` struct is used to generate new existential variables.
/// In the paper, these are mostly notated as α^, α1^, or β^.
/// The `State` is passed around mutably throughout the program.
#[derive(Clone, Debug)]
struct State {
    existentials: usize,
}

impl State {
    /// Creates a new `State` with `existentials` set to 0.
    fn initial() -> State {
        State { existentials: 0 }
    }

    /// Generates a new existential variable and increments the `existentials` counter.
    fn fresh_existential(&mut self) -> String {
        let result = format!("t{}", self.existentials);
        self.existentials += 1;
        result
    }
}

/// Checks if a given `Literal` matches a given `LiteralType`.
fn literal_checks_against(literal: &Literal, type_: &LiteralType) -> bool {
    match (literal, type_) {
        (Literal::Char(_), LiteralType::Char) => true,
        (Literal::String(_), LiteralType::String) => true,
        (Literal::Int(_), LiteralType::Int) => true,
        (Literal::Float(_), LiteralType::Float) => true,
        (Literal::Bool(_), LiteralType::Bool) => true,
        (Literal::Unit, LiteralType::Unit) => true,
        _ => false,
    }
}

/// Figure 11.
/// This function implements the algorithmic typing rules from Figure 11 of the paper.
/// The function takes a mutable reference to the state, a reference to the context, a reference to the expression, and a reference to the type.
/// It returns a new context.
/// The function first prints a helper message, then asserts that the type is well-formed in the context.
/// It then matches on the expression and type to determine which rule to apply.
fn checks_against(
    state: &mut State,
    context: &Context,
    expr: &Expression,
    type_: &Type,
) -> Context {
    print_helper("check", format!("{}", expr), format!("{}", type_), context);
    assert!(is_well_formed(context, type_));
    match (expr, type_) {
        //1I
        // This rule generates no new information and simply propagates the input context.
        (Expression::Literal(lit), Type::Literal(lit_ty)) => {
            print_rule("1I");
            assert!(literal_checks_against(lit, lit_ty));
            context.clone()
        }
        //->I
        // This rule follows the same scheme as the declarative rule: the declarations following x : A are dropped in the conclusion’s output context.
        (Expression::Abstraction(x, e), Type::Function(a, b)) => {
            print_rule("->I");
            let typed_var = ContextElement::TypedVariable(x.clone(), *a.clone());
            let gamma = context.add(typed_var.clone());
            checks_against(state, &gamma, e, b).drop(typed_var)
        }
        //forallI
        // This rule adds a universal type variable α to the (input) context. The output context of the premise allows for some additional (existential) variables to appear after α, in a trailing context Θ. These existential variables could depend on α; since α goes out of scope in the conclusion, we must drop them from the concluding output context, which is just ∆: the part of the premise’s output context that cannot depend on α.
        (_, Type::Quantification(alpha, a)) => {
            print_rule("∀I");
            let var = ContextElement::Variable(alpha.clone());
            let gamma = context.add(var.clone());
            checks_against(state, &gamma, expr, a).drop(var)
        }
        //xI
        // This rule checks the first element of the tuple against the first type, then checks the second element of the tuple against the second type.
        (Expression::Tuple(fst, snd), Type::Product(a, b)) => {
            print_rule("xI");
            let gamma = checks_against(state, context, fst, a);
            checks_against(state, &gamma, snd, b)
        }
        //Sub
        // This rule first synthesizes a type for the expression, then checks that the synthesized type is a subtype of the given type.
        (_, _) => {
            print_rule("Sub");
            let (a, theta) = synthesizes_to(state, context, expr);
            subtype(
                state,
                &theta,
                &apply_context(a, &theta),
                &apply_context(type_.clone(), &theta),
            )
        }
    }
}

/// This function determines the type of a literal.
/// It matches on the literal to determine its type.
fn literal_synthesizes_to(literal: &Literal) -> LiteralType {
    match literal {
        Literal::Char(_) => LiteralType::Char,
        Literal::String(_) => LiteralType::String,
        Literal::Int(_) => LiteralType::Int,
        Literal::Float(_) => LiteralType::Float,
        Literal::Bool(_) => LiteralType::Bool,
        Literal::Unit => LiteralType::Unit,
    }
}

///Figure 11
/// This function implements the algorithmic typing rules from Figure 11.
/// It takes a state, a context, and an expression, and returns a tuple of a type and a context.
/// The function matches on the expression to determine which rule to apply.
fn synthesizes_to(state: &mut State, context: &Context, expr: &Expression) -> (Type, Context) {
    print_helper("synth", format!("{}", expr), "".into(), context);
    match expr {
        //1I=>
        // This rule generates no new information and simply propagates the input context.
        Expression::Literal(lit) => {
            print_rule("1I=>");
            (Type::Literal(literal_synthesizes_to(lit)), context.clone())
        }
        //Var
        // This rule uses an assumption x : A without generating any new information,
        // so the output context in its conclusion Γ ⊢ x ⇒ A ⊣ Γ is just the input context.
        Expression::Variable(x) => {
            print_rule("Var");
            if let Some(annotation) = context.get_annotation(x) {
                return (annotation.clone(), context.clone());
            };
            panic!();
        }
        //Anno
        // This rule does not directly change the context, but the derivation of its premise
        // might include the use of some rule that does, so we propagate the
        // premise’s output context ∆ to the conclusion.
        Expression::Annotation(e, annotation) => {
            print_rule("Anno");
            if is_well_formed(context, annotation) {
                let delta = checks_against(state, context, e, annotation);
                return (annotation.clone(), delta);
            }
            panic!();
        }
        //->I=>
        // This rule corresponds to Decl→I⇒, one of the guessing rules,
        // so we create new existential variables α^ (for the function domain)
        // and β^ (for the codomain) and check the function body against β^.
        // As in ∀App, we do not place a marker before α^, because α^ and β^
        // appear in the output type (λx. e ⇒ α^ → β^).
        Expression::Abstraction(x, e) => {
            print_rule("->I=>");
            let alpha = state.fresh_existential();
            let beta = state.fresh_existential();
            let gamma = context
                .add(ContextElement::Existential(alpha.clone()))
                .add(ContextElement::Existential(beta.clone()))
                .add(ContextElement::TypedVariable(
                    x.clone(),
                    Type::Existential(alpha.clone()),
                ));
            let delta = checks_against(state, &gamma, e, &Type::Existential(beta.clone())).drop(
                ContextElement::TypedVariable(x.clone(), Type::Existential(alpha.clone())),
            );
            return (
                Type::Function(
                    Box::new(Type::Existential(alpha.clone())),
                    Box::new(Type::Existential(beta.clone())),
                ),
                delta,
            );
        }
        //SynthProduct
        // This rule synthesizes a type for a product (tuple) expression.
        // It first synthesizes a type for the first element of the tuple, then synthesizes a type for the second element.
        Expression::Tuple(fst, snd) => {
            print_rule("SynthProduct");
            let (a, gamma) = synthesizes_to(state, context, fst);
            let (b, delta) = synthesizes_to(state, &gamma, snd);
            return (Type::Product(a.into(), b.into()), delta);
        }
        //Let
        // This rule synthesizes a type for a let expression.
        // It first synthesizes a type for the expression being bound, then synthesizes a type for the body of the let expression.
        Expression::Let(var, expr, body) => {
            print_rule("Let");
            let (t0, gamma) = synthesizes_to(state, context, expr);
            let theta = gamma.add(ContextElement::TypedVariable(var.clone(), t0.clone()));

            let (t1, delta) = synthesizes_to(state, &theta, body);
            return (
                t1,
                delta.insert_in_place(ContextElement::TypedVariable(var.clone(), t0), vec![]),
            );
        }

        //->E
        // This rule is the expected analogue of Decl→E; like other rules
        // with two premises, it applies the intermediate context Θ.
        Expression::Application(e1, e2) => {
            print_rule("->E");
            let (a, theta) = synthesizes_to(state, context, e1);
            return application_synthesizes_to(state, &theta, &apply_context(a, &theta), e2);
        }
    }
}

//Figure 11
fn application_synthesizes_to(
    state: &mut State,
    context: &Context,
    type_: &Type,
    expr: &Expression,
) -> (Type, Context) {
    print_helper(
        "app_synth",
        format!("{}", expr),
        format!("{}", type_),
        context,
    );
    match type_ {
        //alphaApp
        Type::Existential(alpha) => {
            print_rule("α^App");
            let alpha1 = state.fresh_existential();
            let alpha2 = state.fresh_existential();
            let gamma = context.insert_in_place(
                ContextElement::Existential(alpha.to_string()),
                vec![
                    ContextElement::Existential(alpha2.clone()),
                    ContextElement::Existential(alpha1.clone()),
                    ContextElement::Solved(
                        alpha.clone(),
                        Type::Function(
                            Box::new(Type::Existential(alpha1.clone())),
                            Box::new(Type::Existential(alpha2.clone())),
                        ),
                    ),
                ],
            );
            let delta = checks_against(state, &gamma, expr, &Type::Existential(alpha1.clone()));
            return (Type::Existential(alpha2.clone()), delta);
        }
        //ForallApp
        Type::Quantification(alpha, a) => {
            print_rule("∀App");
            let alpha1 = state.fresh_existential();
            let gamma = context.add(ContextElement::Existential(alpha1.clone()));
            let substituted_a = substitution(a, alpha, &Type::Existential(alpha1));
            return application_synthesizes_to(state, &gamma, &substituted_a, expr);
        }
        //App
        Type::Function(a, c) => {
            print_rule("->App");
            let delta = checks_against(state, context, expr, a);
            return (*c.clone(), delta);
        }
        _ => panic!(),
    }
}

/// Figure 7
fn is_well_formed(context: &Context, type_: &Type) -> bool {
    match type_ {
        Type::Literal(_) => true,
        Type::Variable(var) => context.has_variable(var),
        Type::Function(a, b) => is_well_formed(context, a) && is_well_formed(context, b),
        Type::Quantification(alpha, a) => {
            is_well_formed(&context.add(ContextElement::Variable(alpha.clone())), a)
        }
        Type::Existential(var) => context.has_existential(var) || context.get_solved(var).is_some(),
        Type::Product(a, b) => is_well_formed(context, a) && is_well_formed(context, b),
    }
}

/// This corresponds to the FV call in Figure 9 Rule <:InstantiateL and <:InstantiateR
/// It checks if a existential variable already occurs in a type to be able to find and panic on cycles
///
/// Alas, I could not find a definition of the FV function and had to copy the implementation of
/// https://github.com/ollef/Bidirectional and https://github.com/atennapel/bidirectional.js
fn occurs_in(alpha: &str, a: &Type) -> bool {
    match a {
        Type::Literal(_) => false,
        Type::Variable(var) => alpha == var,
        Type::Function(t1, t2) => occurs_in(alpha, t1) || occurs_in(alpha, t2),
        Type::Quantification(beta, t) => {
            if alpha == beta {
                return true;
            } else {
                return occurs_in(alpha, t);
            }
        }
        Type::Existential(var) => alpha == var,
        Type::Product(a, b) => occurs_in(alpha, a) || occurs_in(alpha, b),
    }
}

/// Figure 9
fn subtype(state: &mut State, context: &Context, a: &Type, b: &Type) -> Context {
    print_helper("subtype", format!("{}", a), format!("{}", b), context);
    assert!(is_well_formed(context, a));
    assert!(is_well_formed(context, b));
    match (a, b) {
        //<:Unit
        (Type::Literal(lit_a), Type::Literal(lit_b)) => {
            print_rule("<:Unit");
            assert_eq!(lit_a, lit_b);
            context.clone()
        }
        //<:Var
        (Type::Variable(alpha1), Type::Variable(alpha2)) => {
            print_rule("<:Var");
            if is_well_formed(context, a) && alpha1 == alpha2 {
                return context.clone();
            } else {
                panic!();
            }
        }
        //<:Exvar
        (Type::Existential(exist1), Type::Existential(exist2)) if exist1 == exist2 => {
            print_rule("<:Exvar");
            if is_well_formed(context, a) {
                return context.clone();
            } else {
                panic!();
            }
        }
        //<:->
        (Type::Function(a1, a2), Type::Function(b1, b2)) => {
            print_rule("<:->");
            let theta = subtype(state, context, a1, b1);
            return subtype(
                state,
                &theta,
                &apply_context(*a2.clone(), &theta),
                &apply_context(*b2.clone(), &theta),
            );
        }
        (Type::Product(a1, b1), Type::Product(a2, b2)) => {
            print_rule("SubProduct");
            let gamma = subtype(state, context, a1, a2);
            subtype(state, &gamma, b1, b2)
        }
        //<:forallL
        (Type::Quantification(alpha, a), _) => {
            print_rule("<:∀L");
            let r1 = state.fresh_existential();
            let gamma = context
                .add(ContextElement::Marker(r1.clone()))
                .add(ContextElement::Existential(r1.clone()));
            let substituted_a = substitution(a, alpha, &Type::Existential(r1.clone()));
            let delta = subtype(state, &gamma, &substituted_a, b);
            return delta.drop(ContextElement::Marker(r1.clone()));
        }
        //<:forallR
        (_, Type::Quantification(alpha, b)) => {
            print_rule("<:∀R");
            let theta = context.add(ContextElement::Variable(alpha.clone()));
            let delta = subtype(state, &theta, a, b);
            return delta.drop(ContextElement::Variable(alpha.clone()));
        }
        //<:InstatiateL
        (Type::Existential(alpha), _) => {
            print_rule("<:InstantiateL");
            if !occurs_in(alpha, b) {
                instantiate_l(state, context, alpha, b)
            } else {
                panic!("Circular!");
            }
        }
        //<:InstantiateR
        (_, Type::Existential(alpha)) => {
            print_rule("<:InstantiateR");
            if !occurs_in(alpha, a) {
                instantiate_r(state, context, a, alpha)
            } else {
                panic!("Circular!");
            }
        }
        _ => {
            panic!("Couldn't subtype!");
        }
    }
}

/// Figure 10
fn instantiate_l(state: &mut State, context: &Context, alpha: &str, b: &Type) -> Context {
    print_helper("instantiate_l", alpha.into(), format!("{}", b), context);
    let (left_context, right_context) =
        context.split_at(ContextElement::Existential(alpha.to_string()));

    //InstLSolve
    if b.is_monotype() && is_well_formed(&left_context, b) {
        print_rule("InstLSolve");
        return context.insert_in_place(
            ContextElement::Existential(alpha.to_string()),
            vec![ContextElement::Solved(alpha.into(), b.clone())],
        );
    }
    match b {
        //InstLArr
        Type::Function(a1, a2) => {
            print_rule("InstLArr");
            let alpha1 = state.fresh_existential();
            let alpha2 = state.fresh_existential();
            let gamma = context.insert_in_place(
                ContextElement::Existential(alpha.to_string()),
                vec![
                    ContextElement::Existential(alpha2.clone()),
                    ContextElement::Existential(alpha1.clone()),
                    ContextElement::Solved(
                        alpha.into(),
                        Type::Function(
                            Box::new(Type::Existential(alpha1.clone())),
                            Box::new(Type::Existential(alpha2.clone())),
                        ),
                    ),
                ],
            );
            let theta = instantiate_r(state, &gamma, a1, &alpha1);
            let delta = instantiate_l(state, &theta, &alpha2, &apply_context(*a2.clone(), &theta));
            return delta;
        }
        //InstAIIR
        Type::Quantification(beta, b) => {
            print_rule("InstLAllR");
            let delta = instantiate_l(
                state,
                &context.add(ContextElement::Variable(beta.clone())),
                alpha,
                b,
            );
            return delta.drop(ContextElement::Variable(beta.clone()));
        }
        //InstLReach
        Type::Existential(beta) => {
            print_rule("InstLReach");
            return context.insert_in_place(
                ContextElement::Existential(beta.clone()),
                vec![ContextElement::Solved(
                    beta.clone(),
                    Type::Existential(alpha.into()),
                )],
            );
        }
        _ => panic!(),
    }
}

/// Figure 10
fn instantiate_r(state: &mut State, context: &Context, a: &Type, alpha: &str) -> Context {
    print_helper("instantiate_r", format!("{}", a), alpha.into(), context);
    let (left_context, right_context) =
        context.split_at(ContextElement::Existential(alpha.to_string()));

    //InstRSolve
    if a.is_monotype() && is_well_formed(&left_context, a) {
        return context.insert_in_place(
            ContextElement::Existential(alpha.into()),
            vec![ContextElement::Solved(alpha.into(), a.clone())],
        );
    }
    match a {
        //InstRArr
        Type::Function(a1, a2) => {
            print_rule("InstRArr");
            let alpha1 = state.fresh_existential();
            let alpha2 = state.fresh_existential();
            let gamma = context.insert_in_place(
                ContextElement::Existential(alpha.into()),
                vec![
                    ContextElement::Existential(alpha2.clone()),
                    ContextElement::Existential(alpha1.clone()),
                    ContextElement::Solved(
                        alpha.into(),
                        Type::Function(
                            Box::new(Type::Existential(alpha1.clone())),
                            Box::new(Type::Existential(alpha2.clone())),
                        ),
                    ),
                ],
            );
            let theta = instantiate_l(state, &gamma, &alpha1, a1);
            let delta = instantiate_r(state, &theta, &apply_context(*a2.clone(), &theta), &alpha2);
            return delta;
        }
        //InstRAllL
        Type::Quantification(beta, b) => {
            print_rule("InstRAllL");
            let beta1 = state.fresh_existential();
            let gamma = context
                .add(ContextElement::Marker(beta1.clone()))
                .add(ContextElement::Existential(beta1.clone()));
            let delta = instantiate_r(
                state,
                &gamma,
                &substitution(b, beta, &Type::Existential(beta1.clone())),
                alpha,
            );

            return delta.drop(ContextElement::Marker(beta1.clone()));
        }
        Type::Product(a, b) => {
            print_rule("InstRProd");
            let alpha1 = state.fresh_existential();
            let beta1 = state.fresh_existential();
            let gamma = context.insert_in_place(
                ContextElement::Existential(alpha.into()),
                vec![
                    ContextElement::Existential(beta1.clone()),
                    ContextElement::Existential(alpha1.clone()),
                    ContextElement::Solved(
                        alpha.into(),
                        Type::Product(
                            Box::new(Type::Existential(alpha1.clone())),
                            Box::new(Type::Existential(beta1.clone())),
                        ),
                    ),
                ],
            );
            let theta = instantiate_l(state, &gamma, &alpha1, a);
            let delta = instantiate_r(state, &theta, &apply_context(*b.clone(), &theta), &beta1);
            return delta;
        }
        //InstRReach
        Type::Existential(beta) => {
            print_rule("InstRReach");
            return context.insert_in_place(
                ContextElement::Existential(beta.clone()),
                vec![ContextElement::Solved(
                    beta.clone(),
                    Type::Existential(alpha.into()),
                )],
            );
        }
        _ => panic!(),
    }
}

/// Figure 8
fn apply_context(a: Type, context: &Context) -> Type {
    match a {
        Type::Literal(_) => a,
        Type::Variable(_) => a,
        Type::Existential(ref alpha) => {
            if let Some(tau) = context.get_solved(alpha) {
                apply_context(tau.clone(), context)
            } else {
                a
            }
        }
        Type::Function(a, b) => Type::Function(
            Box::new(apply_context(*a, context)),
            Box::new(apply_context(*b, context)),
        ),
        Type::Quantification(alpha, a) => {
            Type::Quantification(alpha, Box::new(apply_context(*a, context)))
        }
        Type::Product(a, b) => Type::Product(
            apply_context(*a, context).into(),
            apply_context(*b, context).into(),
        ),
    }
}

/// Similar to the FV function from subtyping I couldn't find a definition of substitution in the paper
/// Thus I tried to copy the implementation of
/// https://github.com/ollef/Bidirectional and https://github.com/atennapel/bidirectional.js
///
/// Substitution is written in the paper as [α^/α]A which means, α is replaced with α^ in all occurrences in A
fn substitution(a: &Type, alpha: &str, b: &Type) -> Type {
    match a {
        Type::Literal(_) => a.clone(),
        Type::Variable(var) => {
            if var == alpha {
                b.clone()
            } else {
                a.clone()
            }
        }
        Type::Quantification(var, type_) => {
            if var == alpha {
                Type::Quantification(var.clone(), Box::new(b.clone()))
            } else {
                Type::Quantification(var.clone(), Box::new(substitution(type_, alpha, b)))
            }
        }
        Type::Existential(var) => {
            if var == alpha {
                b.clone()
            } else {
                a.clone()
            }
        }
        Type::Product(t1, t2) => Type::Product(
            substitution(t1, alpha, b).into(),
            substitution(t2, alpha, b).into(),
        ),
        Type::Function(t1, t2) => Type::Function(
            Box::new(substitution(t1, alpha, b)),
            Box::new(substitution(t2, alpha, b)),
        ),
    }
}

fn synth(expression: Expression) -> Type {
    let (t, c) = synthesizes_to(&mut State::initial(), &Context::initial(), &expression);
    println!("-------------------RESULTS-------------------");
    println!("{} in context {}", t, c);
    let t = apply_context(t, &c);
    println!("Applied: {}", t);
    // println!("{}", expression);
    println!("-------------------");
    t
}

fn print_helper(fun: &str, c1: String, c2: String, context: &Context) {
    print!(
        "{:<15} {:<85}| {:<25} {:<88}",
        fun,
        c1,
        c2,
        format!("{}", context)
    );
}

fn print_rule(rule: &str) {
    println!("{:>20}", rule);
}

fn literal_string() -> Expression {
    Expression::Literal(Literal::String("Test".into()))
}

fn literal_bool() -> Expression {
    Expression::Literal(Literal::Bool(true))
}

#[test]
fn basic() {
    assert_eq!(synth(literal_string()), Type::Literal(LiteralType::String));
}

#[test]
fn application_string() {
    assert_eq!(
        synth(Expression::Application(
            Expression::Abstraction("x".into(), Expression::Variable("x".into()).into(),).into(),
            literal_string().into(),
        )),
        Type::Literal(LiteralType::String)
    );
}

#[test]
fn application_bool() {
    assert_eq!(
        synth(Expression::Application(
            Expression::Abstraction("x".into(), Expression::Variable("x".into()).into(),).into(),
            literal_bool().into(),
        )),
        Type::Literal(LiteralType::Bool)
    );
}

#[test]
fn lambda() {
    assert_eq!(
        synth(Expression::Abstraction(
            "x".into(),
            Expression::Variable("x".into()).into()
        )),
        Type::Function(
            Type::Existential("t0".into()).into(),
            Type::Existential("t0".into()).into()
        )
    );
}

#[test]
fn idunit() {
    assert_eq!(
        synth(Expression::Application(
            id_fn().into(),
            literal_string().into()
        )),
        Type::Literal(LiteralType::String)
    )
}

#[test]
fn tuples() {
    assert_eq!(
        synth(Expression::Tuple(
            literal_string().into(),
            literal_bool().into()
        )),
        Type::Product(
            Type::Literal(LiteralType::String).into(),
            Type::Literal(LiteralType::Bool).into()
        )
    )
}

#[test]
fn tuples_in_lambda() {
    assert_eq!(
        synth(construct_app(
            Expression::Abstraction(
                "x".into(),
                Expression::Tuple(
                    Expression::Variable("x".into()).into(),
                    Expression::Variable("x".into()).into()
                )
                .into()
            ),
            literal_string()
        )),
        Type::Product(
            Type::Literal(LiteralType::String).into(),
            Type::Literal(LiteralType::String).into(),
        )
    )
}

#[test]
fn nested_tuples() {
    assert_eq!(
        synth(construct_app(
            Expression::Abstraction(
                "x".into(),
                Expression::Tuple(
                    Expression::Variable("x".into()).into(),
                    Expression::Tuple(
                        Expression::Variable("x".into()).into(),
                        Expression::Variable("x".into()).into()
                    )
                    .into()
                )
                .into()
            ),
            literal_string()
        )),
        Type::Product(
            Type::Literal(LiteralType::String).into(),
            Type::Product(
                Type::Literal(LiteralType::String).into(),
                Type::Literal(LiteralType::String).into()
            )
            .into()
        )
    )
}

#[test]
fn tuples_in_fn() {
    assert_eq!(
        synth(Expression::Application(
            id_fn().into(),
            Expression::Tuple(literal_string().into(), literal_bool().into()).into()
        )),
        Type::Product(
            Type::Literal(LiteralType::String).into(),
            Type::Literal(LiteralType::Bool).into()
        )
    )
}

#[test]
fn generalised_let() {
    assert_eq!(
        synth(construct_let(
            "newid",
            id_fn().into(),
            //Without annotation, e.g.
            //Expression::Abstraction("x".into(), Expression::Variable("x".into()).into(),).into(),
            //It fails.
            Expression::Tuple(
                construct_app(
                    Expression::Variable("newid".into()),
                    literal_string().into()
                )
                .into(),
                construct_app(Expression::Variable("newid".into()), literal_bool().into()).into()
            )
        )),
        Type::Product(
            Type::Literal(LiteralType::String).into(),
            Type::Literal(LiteralType::Bool).into()
        )
    )
}

#[test]
fn let_binding() {
    assert_eq!(
        synth(Expression::Let(
            "a".into(),
            literal_bool().into(),
            Expression::Application(id_fn().into(), Expression::Variable("a".into()).into()).into()
        )),
        Type::Literal(LiteralType::Bool)
    )
}

#[test]
fn let_fn() {
    assert_eq!(
        synth(construct_app(
            construct_let(
                "newid",
                Expression::Abstraction("x".into(), Expression::Variable("x".into()).into(),)
                    .into(),
                Expression::Variable("newid".into())
            ),
            literal_string().into()
        )),
        Type::Literal(LiteralType::String)
    );
}

fn construct_app(e0: Expression, e1: Expression) -> Expression {
    Expression::Application(e0.into(), e1.into())
}

fn construct_let(var: &str, e0: Expression, body: Expression) -> Expression {
    Expression::Let(var.into(), e0.into(), body.into())
}

fn id_fn() -> Expression {
    Expression::Annotation(
        Expression::Abstraction("x".into(), Expression::Variable("x".into()).into()).into(),
        Type::Quantification(
            "t".into(),
            Type::Function(
                Type::Variable("t".into()).into(),
                Type::Variable("t".into()).into(),
            )
            .into(),
        ),
    )
}
