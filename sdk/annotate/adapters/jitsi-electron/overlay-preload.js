// Casual Annotate — preload for the OVERLAY window.
//
// Separate from the meeting window's preload because the two windows need opposite halves of the
// bridge: the meeting renderer relays ops outward, the overlay receives them and emits acks.
import { installOverlayBridge } from './preload.js';

installOverlayBridge();
