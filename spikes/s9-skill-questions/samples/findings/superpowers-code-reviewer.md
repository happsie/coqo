### Strengths
- Clear cart model with a single place for the totals (cart.ts:38-49)
- Coupon rules are small pure functions (discounts.ts:20-31)
- Tests cover the volume discount and a percent coupon (2 tests, both pass)

### Issues

#### Critical (Must Fix)
1. **SQL built from user input**
   - File: src/api/orders.ts:48-50
   - Issue: `user.id` is pasted into the query text
   - Fix: use a parameterized query

#### Important (Should Fix)
2. **Cancel does not check the order exists**
   - File: orders.ts:38-42
   - Issue: `order.userId` throws when `get` returns nothing
   - Fix: return 404 first

3. **Sessions measured in the wrong unit**
   - File: auth.ts:16
   - Issue: `createdAt` is in seconds in the db layer; the age is off by 1000×

#### Minor (Nice to Have)
4. **Magic numbers in the discount tiers**
   - File: discounts.ts:10-14
   - Impact: changing a tier means a code change

### Recommendations
- Move the tiers to config.ts, next to MAX_ITEMS_PER_CART
- Consider a retry backoff in createOrder (it retries 3 times back to back)

### Assessment

**Ready to merge: With fixes**

**Reasoning:** The cart logic is solid; the SQL and cancel issues must be fixed first. Review took 4:30 min on commit 3f2c9a1.
