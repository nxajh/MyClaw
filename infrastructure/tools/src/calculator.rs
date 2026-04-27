//! Calculator tool — evaluates mathematical expressions safely.

use async_trait::async_trait;
use capability::tool::{Tool, ToolResult};
use serde_json::json;

/// Simple recursive-descent math expression evaluator.
/// Supports: + - * / () ^ (power) and basic functions: sin, cos, tan, sqrt, abs, ln, log, pi, e.
pub struct CalculatorTool;

impl CalculatorTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CalculatorTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Evaluate a mathematical expression. Supports +, -, *, /, ^ (power), parentheses, and functions: sin, cos, tan, sqrt, abs, ln, log, pi, e."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "The mathematical expression to evaluate, e.g. '2 + 3 * 4' or 'sqrt(144) + pi'."
                }
            },
            "required": ["expression"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let expr = args["expression"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("'expression' is required"))?;

        match eval_math(expr) {
            Ok(result) => Ok(ToolResult {
                success: true,
                output: format!("{} = {}", expr.trim(), result),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("evaluation error: {}", e)),
            }),
        }
    }
}

/// Tokenize and evaluate a math expression.
fn eval_math(input: &str) -> Result<f64, String> {
    let tokens = tokenize(input)?;
    let mut pos = 0;
    parse_expr(&tokens, &mut pos)
}

#[derive(Debug, Clone)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    LParen,
    RParen,
    Ident(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' | '\n' => { i += 1; }
            '+' => { tokens.push(Token::Plus); i += 1; }
            '-' => { tokens.push(Token::Minus); i += 1; }
            '*' => { tokens.push(Token::Star); i += 1; }
            '/' => { tokens.push(Token::Slash); i += 1; }
            '^' => { tokens.push(Token::Caret); i += 1; }
            '(' => { tokens.push(Token::LParen); i += 1; }
            ')' => { tokens.push(Token::RParen); i += 1; }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let num_str: String = chars[start..i].iter().collect();
                let num: f64 = num_str.parse().map_err(|_| format!("invalid number: {}", num_str))?;
                tokens.push(Token::Number(num));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let ident: String = chars[start..i].iter().collect();
                tokens.push(Token::Ident(ident));
            }
            c => return Err(format!("unexpected character: '{}'", c)),
        }
    }

    Ok(tokens)
}

/// Parse expression: addition and subtraction (lowest precedence).
fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut left = parse_term(tokens, pos)?;

    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Plus => {
                *pos += 1;
                let right = parse_term(tokens, pos)?;
                left += right;
            }
            Token::Minus => {
                *pos += 1;
                let right = parse_term(tokens, pos)?;
                left -= right;
            }
            _ => break,
        }
    }

    Ok(left)
}

/// Parse term: multiplication and division.
fn parse_term(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut left = parse_power(tokens, pos)?;

    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Star => {
                *pos += 1;
                let right = parse_power(tokens, pos)?;
                left *= right;
            }
            Token::Slash => {
                *pos += 1;
                let right = parse_power(tokens, pos)?;
                if right == 0.0 {
                    return Err("division by zero".to_string());
                }
                left /= right;
            }
            _ => break,
        }
    }

    Ok(left)
}

/// Parse power: right-associative ^ operator.
fn parse_power(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let base = parse_unary(tokens, pos)?;

    if *pos < tokens.len() && matches!(&tokens[*pos], Token::Caret) {
        *pos += 1;
        let exp = parse_power(tokens, pos)?; // right-associative
        Ok(base.powf(exp))
    } else {
        Ok(base)
    }
}

/// Parse unary: leading minus/plus.
fn parse_unary(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    if *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Minus => {
                *pos += 1;
                let val = parse_unary(tokens, pos)?;
                return Ok(-val);
            }
            Token::Plus => {
                *pos += 1;
                return parse_unary(tokens, pos);
            }
            _ => {}
        }
    }
    parse_primary(tokens, pos)
}

/// Parse primary: number, function call, or parenthesized expression.
fn parse_primary(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    if *pos >= tokens.len() {
        return Err("unexpected end of expression".to_string());
    }

    match &tokens[*pos] {
        Token::Number(n) => {
            let val = *n;
            *pos += 1;
            Ok(val)
        }
        Token::LParen => {
            *pos += 1;
            let val = parse_expr(tokens, pos)?;
            if *pos >= tokens.len() || !matches!(&tokens[*pos], Token::RParen) {
                return Err("expected ')'".to_string());
            }
            *pos += 1;
            Ok(val)
        }
        Token::Ident(name) => {
            let name = name.clone();
            *pos += 1;

            // Check if followed by '(' → function call.
            if *pos < tokens.len() && matches!(&tokens[*pos], Token::LParen) {
                *pos += 1; // skip '('
                let arg = parse_expr(tokens, pos)?;
                if *pos >= tokens.len() || !matches!(&tokens[*pos], Token::RParen) {
                    return Err(format!("expected ')' after function arguments for {}", name));
                }
                *pos += 1; // skip ')'

                match name.to_lowercase().as_str() {
                    "sin" => Ok(arg.to_radians().sin()),
                    "cos" => Ok(arg.to_radians().cos()),
                    "tan" => Ok(arg.to_radians().tan()),
                    "asin" => Ok(arg.asin().to_degrees()),
                    "acos" => Ok(arg.acos().to_degrees()),
                    "atan" => Ok(arg.atan().to_degrees()),
                    "sqrt" => {
                        if arg < 0.0 {
                            return Err("sqrt of negative number".to_string());
                        }
                        Ok(arg.sqrt())
                    }
                    "abs" => Ok(arg.abs()),
                    "ln" => Ok(arg.ln()),
                    "log" | "log10" => Ok(arg.log10()),
                    "log2" => Ok(arg.log2()),
                    "ceil" => Ok(arg.ceil()),
                    "floor" => Ok(arg.floor()),
                    "round" => Ok(arg.round()),
                    "exp" => Ok(arg.exp()),
                    other => Err(format!("unknown function: {}", other)),
                }
            } else {
                // Constant.
                match name.to_lowercase().as_str() {
                    "pi" => Ok(std::f64::consts::PI),
                    "e" => Ok(std::f64::consts::E),
                    "tau" => Ok(std::f64::consts::TAU),
                    "infinity" | "inf" => Ok(f64::INFINITY),
                    other => Err(format!("unknown constant: {}", other)),
                }
            }
        }
        other => Err(format!("unexpected token: {:?}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_arithmetic() {
        assert_eq!(eval_math("2 + 3").unwrap(), 5.0);
        assert_eq!(eval_math("10 - 4").unwrap(), 6.0);
        assert_eq!(eval_math("3 * 4").unwrap(), 12.0);
        assert_eq!(eval_math("10 / 2").unwrap(), 5.0);
    }

    #[test]
    fn test_precedence() {
        assert_eq!(eval_math("2 + 3 * 4").unwrap(), 14.0);
        assert_eq!(eval_math("(2 + 3) * 4").unwrap(), 20.0);
    }

    #[test]
    fn test_power() {
        assert_eq!(eval_math("2 ^ 10").unwrap(), 1024.0);
        assert_eq!(eval_math("2 ^ 3 ^ 2").unwrap(), 512.0); // right-associative: 2^(3^2) = 2^9
    }

    #[test]
    fn test_functions() {
        let result = eval_math("sqrt(144)").unwrap();
        assert!((result - 12.0).abs() < 1e-10);

        let result = eval_math("abs(-5)").unwrap();
        assert!((result - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_constants() {
        let result = eval_math("pi").unwrap();
        assert!((result - std::f64::consts::PI).abs() < 1e-10);

        let result = eval_math("e").unwrap();
        assert!((result - std::f64::consts::E).abs() < 1e-10);
    }

    #[test]
    fn test_complex() {
        let result = eval_math("sqrt(3^2 + 4^2)").unwrap();
        assert!((result - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_unary() {
        assert_eq!(eval_math("-5 + 3").unwrap(), -2.0);
        assert_eq!(eval_math("--5").unwrap(), 5.0);
    }

    #[test]
    fn test_division_by_zero() {
        assert!(eval_math("1 / 0").is_err());
    }
}
