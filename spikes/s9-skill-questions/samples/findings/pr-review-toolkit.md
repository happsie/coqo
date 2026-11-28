# PR Review Summary

## Critical Issues (1 found)
- [silent-failure-hunter]: createOrder rethrows only after the last retry, and swallows the first two errors without logging [src/api/orders.ts:27-31]

## Important Issues (2 found)
- [code-reviewer]: cancelOrder deletes without an existence check [orders.ts:42]
- [pr-test-analyzer]: no test for a coupon below minSubtotal [discounts.ts:21]

## Suggestions (2 found)
- [comment-analyzer]: the comment says "replaces the first one" but nothing tells the user [cart.ts:34]
- [type-design-analyzer]: `db: any` hides every query's shape [src/db/index.ts:2]

## Strengths
- Small, focused modules; `Cart` keeps its items private

## Recommended Action
1. Fix critical issues first
2. Address important issues
3. Consider suggestions
4. Re-run review after fixes
