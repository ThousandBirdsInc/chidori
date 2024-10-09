# This should demonstrate a variety of data marshalling scenarios between different cell types and configurations

## Conversion between Python and Javascript
```python
x0 = 1
x1 = "string"
x2 = [1, 2, 3]
x3 = {"a": 1, "b": 2, "c": 3}
x4 = False
x5 = 1.0
x6 = (1, 2, 3)
x7 = {"a", "b", "c"}

# def example(x):
#     return x

# TODO: marshalling of functions is not currently supported
# x8 = example

# TODO: marshalling of classes is not currently supported
# class ClassExample:
#     def __init__(self, x):
#         self.x = x
#         
# x9 = ClassExample

```

```javascript
Chidori.assertEq(x0, 1);
Chidori.assertEq(x1, "string");
Chidori.assertEq(x2, [1,2,3]);
Chidori.assertEq(x3, {"a": 1, "b": 2, "c": 3});
Chidori.assertEq(x4, false);
Chidori.assertEq(x5, 1.0);
Chidori.assertEq(x6, [1, 2, 3]);

// TODO: marshalling of sets is not currently supported
// Chidori.assertEq(x7, ["c", "b", "a"]);

// Chidori.assertEq(x8, null);

// TODO: marshalling of functions is not currently supported
// Chidori.assertEq(typeof x9, "function");


// These will appear in the UI
const jsX0 = x0;
const jsX1 = x1;
const jsX2 = x2;
const jsX3 = x3;
const jsX4 = x4;
const jsX5 = x5;
const jsX6 = x6;
```


## Conversion between Javascript and Python
```javascript
const y0 = 1;
const y1 = "string";
const y2 = [1, 2, 3];
const y3 = {a: 1, b: 2, c: 3};
const y4 = false;
const y5 = 1.0;
const y6 = [1, 2, 3];
// TODO: marshalling of sets is not currently supported
// const y7 = new Set(["a", "b", "c"]);

// TODO: marshalling of functions is not currently supported
// const y8 = (y) => y;

// TODO: marshalling of classes is not currently supported
// class ClassExample {
//    constructor(y) {
//        this.y = y;
//    }
//};
//const y9 = ClassExample;
```

```python
import unittest

class TestMarshalledValues(unittest.TestCase):
    def test_all(self):
        self.assertEqual(y0, 1)
        self.assertEqual(y1, "string")
        self.assertEqual(y2, [1,2,3])
        self.assertEqual(y3, {"a": 1, "b": 2, "c": 3})
        self.assertEqual(y4, False)
        self.assertEqual(y5, 1.0)
        self.assertEqual(y6, [1,2,3])

unittest.TextTestRunner().run(unittest.TestLoader().loadTestsFromTestCase(TestMarshalledValues))


# These will appear in the UI
pyY0 = y0
pyY1 = y1
pyY2 = y2
pyY3 = y3
pyY4 = y4
pyY5 = y5
pyY6 = y6
```

