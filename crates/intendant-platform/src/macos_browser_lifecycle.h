// Pure ownership state used by the native launcher and non-GUI regression tests.
#ifndef INTENDANT_MACOS_BROWSER_LIFECYCLE_H
#define INTENDANT_MACOS_BROWSER_LIFECYCLE_H
#include <stdbool.h>
enum { BrowserPublish = 1, BrowserTerminate = 2, BrowserForce = 4, BrowserDone = 8 };
typedef struct { bool stopping, published, terminate_sent, force_sent; } BrowserLifecycle;
static inline unsigned browser_lifecycle_step(BrowserLifecycle *s, bool launch_finished,
        bool has_app, bool terminated, bool stop, bool force_due) {
    s->stopping |= stop;
    if (!launch_finished) return 0; // A deadline is not proof of launch failure.
    if (!has_app || terminated) return BrowserDone;
    if (s->stopping) {
        if (!s->terminate_sent) { s->terminate_sent = true; return BrowserTerminate; }
        if (force_due && !s->force_sent) { s->force_sent = true; return BrowserForce; }
        return 0;
    }
    if (!s->published) { s->published = true; return BrowserPublish; }
    return 0;
}
#ifdef INTENDANT_BROWSER_LIFECYCLE_TEST
#include <assert.h>
#include <stdio.h>
int main(void) {
    unsigned checks = 0;
#define CHECK(x) do { assert(x); ++checks; } while (0)
    for (int cancel = 0; cancel != 2; ++cancel) {
        BrowserLifecycle s = {0};
        CHECK(browser_lifecycle_step(&s, false, false, false, cancel, false) == 0);
        if (cancel) {
            CHECK(browser_lifecycle_step(&s, false, false, false, false, true) == 0);
            CHECK(browser_lifecycle_step(&s, true, true, false, false, false) == BrowserTerminate);
            CHECK(!s.published);
        } else {
            CHECK(browser_lifecycle_step(&s, true, true, false, false, false) == BrowserPublish);
            CHECK(browser_lifecycle_step(&s, true, true, false, false, false) == 0);
            CHECK(browser_lifecycle_step(&s, true, true, false, true, false) == BrowserTerminate);
        }
        CHECK(browser_lifecycle_step(&s, true, true, false, true, false) == 0);
        CHECK(browser_lifecycle_step(&s, true, true, false, true, true) == BrowserForce);
        CHECK(browser_lifecycle_step(&s, true, true, false, true, true) == 0);
        CHECK(browser_lifecycle_step(&s, true, true, true, true, true) == BrowserDone);
    }
    BrowserLifecycle failed = {0};
    CHECK(browser_lifecycle_step(&failed, true, false, false, false, false) == BrowserDone);
    BrowserLifecycle ended = {0};
    CHECK(browser_lifecycle_step(&ended, true, true, true, false, false) == BrowserDone);
    CHECK(!ended.published);
    printf("{\"passed\":%u,\"native_launches\":0,\"input_posts\":0}\n", checks);
    return 0;
}
#endif
#endif
