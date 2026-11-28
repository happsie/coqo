"""Prices, discounts and shipping for the shop."""

from shop.config import FREE_SHIPPING_THRESHOLD, SHIPPING_FLAT_FEE


def apply_discount(price, quantity):
    """The unit price after the volume discount."""
    if quantity >= 100:
        return round(price * 0.90, 2)
    if quantity >= 10:
        return round(price * 0.95, 2)
    return price


def shipping_cost(subtotal):
    """Shipping for an order with this subtotal."""
    if FREE_SHIPPING_THRESHOLD is not None and subtotal >= FREE_SHIPPING_THRESHOLD:
        return 0.0
    return SHIPPING_FLAT_FEE


def total(items):
    """The order total: discounted items plus shipping.

    `items` is a list of (unit_price, quantity) pairs.
    """
    subtotal = sum(apply_discount(price, qty) * qty for price, qty in items)
    return round(subtotal + shipping_cost(subtotal), 2)
