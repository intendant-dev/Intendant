#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
@interface IntendantPatternFixtureView : NSView
@end
@implementation IntendantPatternFixtureView
-(void)drawRect:(NSRect)rect {
    CGFloat w=self.bounds.size.width,h=self.bounds.size.height;
    [[NSColor colorWithSRGBRed:1 green:0 blue:0 alpha:1] setFill]; NSRectFill(NSMakeRect(0,h/2,w/2,h/2));
    [[NSColor colorWithSRGBRed:0 green:1 blue:0 alpha:1] setFill]; NSRectFill(NSMakeRect(w/2,h/2,w/2,h/2));
    [[NSColor colorWithSRGBRed:0 green:0 blue:1 alpha:1] setFill]; NSRectFill(NSMakeRect(0,0,w/2,h/2));
    [[NSColor whiteColor] setFill]; NSRectFill(NSMakeRect(w/2,0,w/2,h/2));
}
@end
int main(int argc, const char **argv) {
 @autoreleasepool {
    if(argc!=3) return 2;
    char *end=NULL; unsigned long raw=strtoul(argv[1],&end,10);
    if(!end||*end||!raw||raw>UINT32_MAX||raw==CGMainDisplayID()) return 3;
    NSApplication *app=[NSApplication sharedApplication];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    NSScreen *selected=nil;
    for(NSScreen *s in [NSScreen screens])
      if([s.deviceDescription[@"NSScreenNumber"] unsignedIntValue]==raw) selected=s;
    if(!selected) return 4;
    NSPanel *p=[[NSPanel alloc] initWithContentRect:selected.frame
      styleMask:NSWindowStyleMaskBorderless|NSWindowStyleMaskNonactivatingPanel
      backing:NSBackingStoreBuffered defer:NO screen:selected];
    p.releasedWhenClosed=NO; p.hidesOnDeactivate=NO;
    p.level=NSFloatingWindowLevel; p.opaque=YES; p.hasShadow=NO;
    p.contentView=[[IntendantPatternFixtureView alloc] initWithFrame:NSMakeRect(0,0,selected.frame.size.width,selected.frame.size.height)];
    [p setFrame:selected.frame display:NO]; [p orderFrontRegardless]; [p displayIfNeeded];
    NSData *data=[NSJSONSerialization dataWithJSONObject:@{@"native_id":@(raw),@"window_id":@(p.windowNumber),@"pid":@([[NSProcessInfo processInfo] processIdentifier])} options:0 error:nil];
    if(![data writeToFile:[NSString stringWithUTF8String:argv[2]] atomically:YES]) return 5;
    [NSTimer scheduledTimerWithTimeInterval:40 repeats:NO block:^(NSTimer *t){[p close];[app terminate:nil];}];
    [app run];
 }
 return 0;
}