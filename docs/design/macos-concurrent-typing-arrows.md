# Status: native concurrency policy deferred

The proposed native CGEvent policy below was not positively accepted under concurrent
human typing. The browser-keyboard continuation restores the main-branch native
held-key and exact human-focus guards in a forward commit. Historical commits and
the evidence harness remain available; browser-page CDP tests are not acceptance
for these native policy changes. The current deliverable is independently targeted
managed-browser text and keyboard input, not a separate OS input seat.

---

# macOS concurrent ordinary-key typing with bound horizontal arrows

This slice changes only the readiness policy for the existing retained-receiver
ArrowLeft/ArrowRight native island. The event itself remains a fixed unmodified
horizontal arrow, created from a private CGEventSource, addressed to the exact
retained PID/window, and checked to have zero Quartz modifier flags, the exact
SDK keycode/function-key character, no autorepeat, and the expected private
source/tag before it can be posted.

Ordinary human key-down state is no longer a readiness conflict for this fixed
background arrow. This is required for concurrent local typing on the foreground
desktop: the existing implementation rejected whenever any physical key was
held, even though the user focus remained on a different application and the
agent event was exact-target, private-source, and unmodified.

Shortcut modifiers remain a hard refusal: Shift, Control, Option, Command, and
Fn. Held human mouse buttons also remain a refusal in this slice. Caps Lock
remains a permitted latched nontext state. Existing Accessibility/PostEvent
permission, target-background, process/window/monitor identity, receiver role,
protection, ancestry, geometry, human-focus, one-use token, expiry, before/after
revalidation, partial-effect accounting, and no-automatic-replay rules are
unchanged.

The readiness check is repeated at preparation, before native construction,
immediately inside the one-shot post path, and after possible posting. A human
shortcut modifier appearing before posting therefore still refuses. A change
after possible posting remains uncertain evidence and is never corrected or
replayed. This is not an atomic OS input seat and does not prove continuous
isolation.

Native tests must pin the conflict classifier: ordinary/no-modifier state is
allowed; Caps Lock is allowed; each shortcut modifier is refused; each of the
five mouse-button bits is refused; combined conflicts are refused. Existing
constructor/readback/exception/replay tests remain nonposting.

Live acceptance must use the exact published build, a disposable browser on an
owned virtual monitor, and independent focus/activity evidence. The coordinated
foreground user should type lowercase text without shortcut modifiers. Positive
acceptance requires exactly one trusted background arrow pair/effect, unchanged
synthetic text, known unchanged human/receiver/foreground endpoints, target not
observed foreground, independently observed keyboard activity beyond the tested
pair, and confirmed browser/profile/display cleanup. No retry-until-success.

This slice does not add general text, additional keys, chords, activation,
hidden focus clicks, global posting, pointer concurrency, streaming, or a new
input authority surface.
