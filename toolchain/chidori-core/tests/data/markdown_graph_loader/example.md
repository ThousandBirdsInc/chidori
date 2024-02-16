# Example Project Documentation

## Overview

This document outlines sample Python and JavaScript code snippets for demonstration purposes.

### Python Example

Below is a simple Python function that calculates the factorial of a number using recursion.

```python
def factorial(n):
    """Calculate the factorial of a number."""
    if n == 1:
        return 1
    else:
        return n * factorial(n-1)

# Example usage
print(factorial(5))
```


JavaScript Example
Here's a JavaScript function that prints Fibonacci numbers up to a given count.
```javascript
function printFibonacci(count) {
    let n1 = 0, n2 = 1, nextTerm;

    console.log('Fibonacci Series:');

    for (let i = 1; i <= count; i++) {
        console.log(n1);
        nextTerm = n1 + n2;
        n1 = n2;
        n2 = nextTerm;
    }
}

// Example usage
printFibonacci(5);
```