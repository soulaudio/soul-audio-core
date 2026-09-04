// Shared browser-test fixture: a synchronous, manually-triggered stand-in for
// `IntersectionObserver`. Real Chromium's observer is asynchronous and timed
// to actual layout/scroll, which would make "is this node visible yet"
// nondeterministic in a headless run. Install with:
//
//   vi.stubGlobal("IntersectionObserver", FakeIntersectionObserver);
//
// and fire `FakeIntersectionObserver.instances.at(-1)?.fire(true)` (or index
// into `instances` for multiple observed nodes) to drive visibility directly.
export class FakeIntersectionObserver {
	static instances: FakeIntersectionObserver[] = [];
	cb: IntersectionObserverCallback;
	disconnected = false;
	constructor(cb: IntersectionObserverCallback) {
		this.cb = cb;
		FakeIntersectionObserver.instances.push(this);
	}
	observe(): void {}
	disconnect(): void {
		this.disconnected = true;
	}
	fire(isIntersecting: boolean): void {
		this.cb(
			[{ isIntersecting } as IntersectionObserverEntry],
			this as unknown as IntersectionObserver,
		);
	}
}
