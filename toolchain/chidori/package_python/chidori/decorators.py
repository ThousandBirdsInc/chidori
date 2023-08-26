import inspect
import asyncio
from ._chidori import Chidori, GraphBuilder

# TODO: API KEYS: os.environ["OPEN_AI_KEY"] = "1"

# TODO: Tighter feedback loop: we need node output in-terminal (at least behind a DEBUG flag)

# TODO: abstract away chidori start/run methods?

c = Chidori("0", "http://localhost:9800")

# TODO: Consider the commit message -- expose commit as method?
graph_builder = GraphBuilder()


def check_config(config_dict, node_type):
    CONFIG_TYPES = {
        "prompt_node": list(
            inspect.signature(graph_builder.prompt_node).parameters.keys()
        ),
    }

    if not isinstance(config_dict, dict):
        raise Exception(
            f"Node configuration function must return a dictionary of node config keys and values. Config keys for {node_type} are {CONFIG_TYPES[node_type]}. Config sent: {config_dict}"
        )


def prompt_node(node_config_function):
    config_dict = node_config_function()
    check_config(config_dict, "prompt_node")

    if config_dict.get("name") is None:
        # implicitly name based on function definition
        config_dict["name"] = node_config_function.__name__

    return graph_builder.prompt_node(**config_dict)  # maybe key mismatch gets ignored?


# def custom_node(func):
#     # Registers this node at definition time
#     # But we want to check if the node has been previously registered as well
#     c.register_custom_node_handle(func.__name__, func)

#     # Can return a class that includes Queries and Output
#     # For the sake of inference
#     g.custom_node(
#         name=func.__name__,
#     )

#     # Return the func? No, return the node handle,
#     # This function isn't meant to be executed by the user
#     nh = NodeHandle(name=func.__name__)
#     return func
