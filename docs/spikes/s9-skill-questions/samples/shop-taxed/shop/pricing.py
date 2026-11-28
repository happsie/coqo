"""Prices, discounts and shipping for the shop."""

from shop.config import FREE_SHIPPING_THRESHOLD, SHIPPING_FLAT_FEE, TAX_RATE


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


def apply_tax(amount, rate):
    """`amount` with tax at `rate` (a fraction) added."""
    if rate < 0:
        raise ValueError(f"tax rate must not be negative: {rate}")
    return amount * (1 + rate)


def total(items, tax_rate=None):
    """The order total: discounted items plus shipping, then tax.

    `items` is a list of (unit_price, quantity) pairs. `tax_rate` defaults
    to the shop's `TAX_RATE`.
    """
    rate = TAX_RATE if tax_rate is None else tax_rate
    subtotal = sum(apply_discount(price, qty) * qty for price, qty in items)
    return round(apply_tax(subtotal + shipping_cost(subtotal), rate), 2)
