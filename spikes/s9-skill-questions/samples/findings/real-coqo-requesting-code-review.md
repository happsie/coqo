All 4 tests pass. I'm writing up the review now.

## Code review: uncommitted change vs `HEAD` (bd55d97)

I did this review myself in one pass, not with a subagent. I filled in the skill's `code-reviewer.md` template and followed its rules. No plan or spec file came with this change, so I took the requirements from the diff, the docstrings and the config comment: add VAT, configured by `TAX_RATE`, to every order total, applied after shipping, and refuse a negative rate. I read `shop/config.py`, `shop/pricing.py`, `shop/orders.py` (the only caller of `total`) and `test_pricing.py`, and ran the tests: **4 passed**.

---

### Strengths
- **Existing behaviour doesn't change.** `TAX_RATE = 0.0` (`shop/config.py:13`) means current totals and `Order.amount()` give the same results as before. The existing `test_total_adds_shipping` still passes.
- **An explicit zero rate works.** `total()` checks `tax_rate is None` (`shop/pricing.py:36`), not whether the value is falsy, so `tax_rate=0` is honoured.
- **Tax is its own function.** `apply_tax` (`shop/pricing.py:22-26`) is small, has a docstring and checks its input. A negative rate raises a `ValueError` with a clear message.
- **The docstring matches the code.** It says tax is applied after shipping, which is what happens.
- **Both new paths have tests:** a positive rate and a refused negative rate.

### Issues

#### Critical (Must Fix)
None.

#### Important (Should Fix)

1. **The new test expects a half-cent to round down**
   - File: `test_pricing.py:17`, `shop/pricing.py:37`
   - What's wrong: (10.00 + 4.90) × 1.25 is exactly 18.625. Python's `round()` sends an exact half to the even digit, so it returns **18.62**. Standard commercial rounding (half-up) gives **18.63**. The test asserts 18.62, which locks in round-half-even as the shop's tax rounding policy without saying so. Floats make this worse: whether a value lands exactly on .xx5 depends on how it's stored in binary, so a total near a half-cent can go either way.
   - Why it matters: VAT rounding rules depend on the jurisdiction, and a total that's a cent short shows up in reconciliation and on invoices. It also feeds the refund ceiling in `orders.py`.
   - How to fix: decide the rounding rule on purpose. Then use `Decimal` with `quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)`, or whichever rule applies, in `total`/`apply_tax`, and assert the value that rule gives. Which rule to use is a business decision.

2. **`Order.amount()` recalculates with whatever `TAX_RATE` is current**
   - File: `shop/orders.py:17-18`, `:32`, `:35`
   - What's wrong: `Order` doesn't store what the customer paid. `amount()` calls `total(self.items)` again every time, and that now uses the current `TAX_RATE`. If the rate changes between an order being placed and refunded (going live with 25 % VAT, say), the refund ceiling and the "fully refunded" check use the new rate. You could refund more than was paid, or never reach `"refunded"` status.
   - Why it matters: the same weakness already existed for `SHIPPING_FLAT_FEE`. This change adds a setting that is far more likely to change (it ships at 0.0 and is clearly going to be set later), and its effect is proportional to the order size.
   - How to fix: record the total, or at least the tax rate, on the `Order` when it is placed, and make `amount()` return that.

3. **A rate given as a percentage gets through**
   - File: `shop/pricing.py:24`
   - What's wrong: only `rate < 0` is rejected. `tax_rate=25`, meaning 25 %, is accepted and turns a 14.90 order into **387.40** (I checked this). The config comment says it's a fraction, but writing a percentage is an obvious mistake for a caller or whoever edits the config.
   - How to fix: also reject `rate >= 1`, or set an upper bound that suits you, with the same `ValueError`. Add a test for it.

#### Minor (Nice to Have)

1. **`TAX_RATE` is read once at import.** `from shop.config import TAX_RATE` (`shop/pricing.py:3`) copies the value when the module loads. Changing or monkeypatching `shop.config.TAX_RATE` later has no effect on `total()`. The shipping settings already work this way, but if the rate will be set at runtime, read `config.TAX_RATE` inside `total()`.
2. **Test gaps.** No test runs the default-rate path with a non-zero `TAX_RATE`, `apply_tax` has no direct test, and nothing covers tax together with free shipping.
3. **README.** The usage section in `README.md` doesn't mention `tax_rate` or `TAX_RATE`.
4. **Equality check on floats in `process_refund`** (`shop/orders.py:35`). This was already there, but non-zero tax makes totals with long float tails more likely, which makes `refunded == amount()` less reliable. Storing amounts as `Decimal` (Important #1) fixes this too.

### Declined to judge
- **Whether VAT applies to shipping.** The code taxes the subtotal plus shipping. That matches the usual EU rule that shipping follows the VAT of the goods, but it's a business and legal decision, not something I can check from the code.
- **Whether `unit_price` is net or gross.** The change assumes prices are net and adds VAT on top. Consumer prices in the EU are often shown with VAT included. That's a product decision the spec doesn't cover.
- **Different rates per item or category** (reduced VAT rates). The spec describes a single rate for the whole order, so I didn't count this as missing.
- **Currency-specific rounding.** `CURRENCY` is only EUR, so I only judged rounding to 2 decimal places.

### Recommendations
- Use `Decimal` for money in `pricing.py` and `orders.py` and set the rounding rule explicitly. That fixes Important #1 and Minor #4 together.
- Store the price an order was charged at, rather than recalculating it from live config.

### Assessment

**Ready to merge?** With fixes

**Reasoning:** The change is small, doesn't alter existing totals and is tested. But the new test quietly sets the rounding rule to round-half-even, which gives 18.62 where half-up gives 18.63. And once `TAX_RATE` isn't zero, refunds will use the current rate instead of the one the customer paid. Both should be settled before tax is switched on.

---

The rounding rule in Important #1, and the two items under "Declined to judge" (whether shipping is taxed, and whether prices are net or gross), are your decisions to make. Tell me which way you want each, and whether I should fix the Important items.
