This example sketches a memory-safe Rust handle to a C++ object whose
lifetime C++ alone controls, modelled on Chromium's `RenderFrameHost` -
though none of the C++ here is real Chromium code, only enough of a fake to
experiment against. The handle registers a `WebContentsObserver` subclass so
it hears about the object's destruction, and hands out borrows that either
prove the object is still alive or tell you plainly that it isn't.
