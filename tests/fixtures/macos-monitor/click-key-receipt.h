// Fixture-only DOM acknowledgement. The owner checks a fresh challenge and
// exact plan, but cannot authenticate DOM provenance. This is not IAM authority.
#pragma once
typedef struct {
    BrowserPointerPlan click;
    int64_t key_tag;
    uint64_t challenge;
    uint32_t version, click_count;
    CGPoint client;
    CGSize viewport;
} BrowserClickReceipt;
_Static_assert(sizeof(BrowserClickReceipt) == 136, "click receipt wire layout");
typedef struct {
    BOOL attempted, available;
    int64_t key_tag;
} ClickReceiptState;
static BOOL click_receipt_valid(BrowserClickReceipt r, BrowserPointerPlan click, uint64_t challenge) {
    double v[]={r.client.x,r.client.y,r.viewport.width,r.viewport.height};
    for(unsigned i=0;i<4;i++) if(!isfinite(v[i]) || fabs(v[i])>1000000) return NO;
    return r.version==1 && r.click_count==1 && challenge>0 && r.challenge==challenge &&
        r.key_tag>0 && r.key_tag!=click.tag && browser_pointer_valid(r.click) &&
        r.click.version==click.version && r.click.window==click.window && r.click.tag==click.tag &&
        CGRectEqualToRect(r.click.bounds,click.bounds) && CGPointEqualToPoint(r.click.global,click.global) &&
        CGPointEqualToPoint(r.click.local,click.local) && r.viewport.width==click.bounds.size.width &&
        r.viewport.height>=200 && r.viewport.height<=click.bounds.size.height &&
        click.bounds.size.height-r.viewport.height<=200 && r.client.x>=0 && r.client.y>=0 &&
        r.client.x<r.viewport.width && r.client.y<r.viewport.height &&
        r.client.x==click.local.x &&
        r.client.y+click.bounds.size.height-r.viewport.height==click.local.y;
}
static BOOL click_receipt_accept(ClickReceiptState *state, BrowserClickReceipt receipt,
                                 BrowserPointerPlan click, uint64_t challenge, BOOL permitted) {
    if(!state) return NO;
    BOOL fresh=!state->attempted; state->attempted=YES; state->available=NO;
    if(!fresh || !permitted || !click_receipt_valid(receipt,click,challenge)) return NO;
    state->key_tag=receipt.key_tag; state->available=YES; return YES;
}
static BOOL click_receipt_take(ClickReceiptState *state, int64_t tag) {
    if(!state) return NO;
    BOOL available=state->available; state->available=NO;
    return available && tag>0 && state->key_tag==tag;
}
static BOOL click_receipt_read(BrowserClickReceipt *p) {
    struct stat info;
    if(fstat(STDIN_FILENO,&info)!=0 || !S_ISFIFO(info.st_mode)) return NO;
    size_t have=0; NSTimeInterval end=NSProcessInfo.processInfo.systemUptime+1;
    while(have<sizeof(*p) && NSProcessInfo.processInfo.systemUptime<end) {
        struct pollfd fd={STDIN_FILENO,POLLIN,0};
        if(poll(&fd,1,20)<0) return NO;
        if(!(fd.revents&(POLLIN|POLLHUP))) continue;
        ssize_t n=read(STDIN_FILENO,(char *)p+have,sizeof(*p)-have);
        if(n<=0) return NO;
        have+=(size_t)n;
    }
    return have==sizeof(*p);
}
