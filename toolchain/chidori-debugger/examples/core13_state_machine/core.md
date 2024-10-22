# Demonstrating dependency management


```python
from enum import Enum, auto

class State(Enum):
    WAITING = auto()
    ITEM_SELECTED = auto()
    PAYMENT_RECEIVED = auto()
    DISPENSING_ITEM = auto()

class VendingMachine:
    def __init__(self):
        self.state = State.WAITING
        self.selected_item = None
        self.payment = 0

    def select_item(self, item):
        if self.state == State.WAITING:
            self.selected_item = item
            self.state = State.ITEM_SELECTED
            print(f"Item selected: {item}")
        else:
            print("Please wait, transaction in progress")

    def insert_payment(self, amount):
        if self.state == State.ITEM_SELECTED:
            self.payment += amount
            print(f"Payment received: ${amount}")
            if self.payment >= 1.00:  # Assuming all items cost $1.00
                self.state = State.PAYMENT_RECEIVED
        else:
            print("Please select an item first")

    def dispense_item(self):
        if self.state == State.PAYMENT_RECEIVED:
            self.state = State.DISPENSING_ITEM
            print(f"Dispensing {self.selected_item}")
            self.state = State.WAITING
            self.selected_item = None
            self.payment = 0
        else:
            print("Unable to dispense item")

    def cancel_transaction(self):
        if self.state != State.WAITING:
            print("Transaction cancelled")
            if self.payment > 0:
                print(f"Returning ${self.payment}")
            self.state = State.WAITING
            self.selected_item = None
            self.payment = 0
        else:
            print("No transaction in progress")

    def run(self):
        while True:
            print(f"\nCurrent state: {self.state}")
            action = input("Enter action (select/pay/dispense/cancel/quit): ").lower()

            if action == "select":
                item = input("Enter item name: ")
                self.select_item(item)
            elif action == "pay":
                amount = float(input("Enter payment amount: "))
                self.insert_payment(amount)
            elif action == "dispense":
                self.dispense_item()
            elif action == "cancel":
                self.cancel_transaction()
            elif action == "quit":
                print("Exiting vending machine")
                break
            else:
                print("Invalid action")
```
