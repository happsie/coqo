What's prompting free shipping on big orders right now, and how would you know it's working?

I'm asking because the answer decides where "big" starts and what it's measured on. The check already runs on the subtotal *after* the volume discount, so a discount can push an order back under the line. Whether that's fine depends on what you want free shipping to do for you.
Your shop already supports this. `FREE_SHIPPING_THRESHOLD` in `shop/config.py:8` is set to `None` right now, so free shipping is off. `shipping_cost` in `shop/pricing.py:15` already drops the €4.90 fee for any order whose subtotal reaches that number. So the code side is nearly a one-line change. The real decision is what counts as "big."

You want customers with large orders to stop paying the €4.90 flat fee. I've left you a question in the editor at `shop/config.py:7` about why you want this now, because the answer shapes where the line goes.

While reading I found three things that affect this:

- **Discounts count first.** The threshold is checked against the subtotal after the 5% and 10% volume discounts. Someone buying 10 or more of an item gets a smaller subtotal, and that could drop them just under the free-shipping line.
- **It changes old orders too.** `Order.amount()` in `shop/orders.py:17` recalculates the total from today's settings. If you switch on or change the threshold, the maximum refund on orders that were already placed changes along with it.
- **A test may need updating.** `test_pricing.py:10` expects a €10 order to pay shipping. That still passes unless the line is set at €10 or lower.

Tell me if I've got any of this wrong.
