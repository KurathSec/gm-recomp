// zlib for the converter in the browser: the two sync calls it makes, on pako.
import { Buffer } from 'buffer';
import pako from 'pako';

export const inflateSync = (data) => Buffer.from(pako.inflate(data));
export const deflateSync = (data, options) => Buffer.from(pako.deflate(data, options?.level !== undefined ? { level: options.level } : undefined));
export default { inflateSync, deflateSync };
