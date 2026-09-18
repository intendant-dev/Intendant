# Raw pointer delivery acceptance

Follow-up to #940. Exercise the opt-in own-process receiver, then isolate a cross-process receiver only if exact routing is established. Preserve nonposting defaults, bounded receipts, owner activity attribution, target-only posting and cleanup. No production input or global event fallback is introduced.
