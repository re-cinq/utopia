// Whether the view follows downward as the answer is written out.
//
// Previously every change to `shown` scrolled the bottom sentinel into view, and during generation liveAnswer notifies every 33ms:
// a person scrolled up reading earlier text (trace, previous answer, citations) was yanked back to the bottom every 33ms. An answer still being written
// made everything else on screen unreadable.
//
// The criterion is now **where the person is right now**: if they are at the bottom, follow the newly written text; if they scrolled up, stay put and, once they scroll back
// to the bottom, resume following. 'Bottom' has some slack: the list has pb-12 padding below, and line wrapping and zoom can also leave the last
// few pixels misaligned. Requiring an exact match would count someone who stays at the bottom as scrolled up and lose the follow.

/** How close to the bottom counts as 'still reading along', in CSS pixels */
export const FOLLOW_SLACK_PX = 48;

/** Whether this scroll container is currently at the bottom. Always true when the content is shorter than the viewport: there is nowhere to scroll away to */
export function followsBottom(el: {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
}): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_SLACK_PX;
}
