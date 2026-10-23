// crypto for the converter in the browser: createHash('md5') only.
import { Buffer } from 'buffer';
import md5 from 'md5';

export function createHash(alg) {
  if (alg !== 'md5') throw new Error(`crypto: ${alg} is not available`);
  const parts = [];
  return {
    update(d) {
      parts.push(Buffer.from(d));
      return this;
    },
    digest(enc) {
      const hex = md5(Buffer.concat(parts));
      return enc === 'hex' ? hex : Buffer.from(hex, 'hex');
    },
  };
}
export default { createHash };
