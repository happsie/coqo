"""Orders: placing them, cancelling them and paying money back."""

from datetime import date, timedelta

from shop.config import REFUND_WINDOW_DAYS
from shop.pricing import total


class Order:
    def __init__(self, order_id, items, placed_on):
        self.order_id = order_id
        self.items = items
        self.placed_on = placed_on
        self.status = "placed"
        self.refunded = 0.0

    def amount(self):
        return total(self.items)


def cancel(order):
    if order.status != "placed":
        raise ValueError(f"order {order.order_id} cannot be cancelled: {order.status}")
    order.status = "cancelled"


def process_refund(order, amount, today=None):
    """Pay `amount` back for `order`, within the refund window."""
    today = today or date.today()
    if today - order.placed_on > timedelta(days=REFUND_WINDOW_DAYS):
        raise ValueError("the refund window has closed")
    if order.refunded + amount > order.amount():
        raise ValueError("cannot refund more than was paid")
    order.refunded += amount
    order.status = "refunded" if order.refunded == order.amount() else "partly refunded"
    return order.refunded
