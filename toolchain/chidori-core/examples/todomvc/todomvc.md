# This is an example of an agent that maintains a todo list for a user.

First we're defining a series of functions for managing internal state and interacting
with the todo data. These could instead manipulate a database, or interact with a web service.

You'll notice that there are some additional decorators on the functions. These are used to provide
hooks for us to extend the functionality of the agent. 
```python
import chidori as ch
todo_list = []

@ch.emit("todo_added")
def add_todo(todo: str):
    todo_list.append(todo)

def remove_todo(idx: int):
    todo_list.pop(idx)
    
def list_todos():    
    return todo_list
```


Now we're getting into the agent itself. We're providing the methods that we previously defined to a prompt.
Some LLM providers make available a structured syntax for function invocations. When possible, we leverage that. 
We allow the user to inject references to defined functions which we analyze and use to generate the 
appropriate syntax for the LLM provider.

We use yaml frontmatter as a configuration interface for cells. 
In this example we're using openai and the gpt-3.5-turbo model.
```prompt (todoagent)
---
provider: openai
model: gpt-3.5-turbo
---
{#system}
    {{add_todo}}
    {{remove_todo}}
    {{list_todos}}
{/system}
{#user}
    {{user_input}}
{/user}
```

Lets add more logic to the agent. 
When a todo item is added, we want to assess the priority of the item and store that as well.

To expose this in a way we can interact with it, we have a few choices:
* we can use a REPL node to handle text input and output
* we can use a web node to provide a JSON API interface
* or we can use a web node to provide a UI

We're going to demonstrate all 3.

```web
---
port: 3838
---
POST /api/todo_message todoagent
GET /ui/chat chat
```

```html (chat)
<html>
  <input name="todo"/>
</html
<style> </style>
```
