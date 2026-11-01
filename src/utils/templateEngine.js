import { usernameFromEmail } from './usernameFromEmail.js';

const TOKEN = /^[A-Z0-9]{8,12}$/;

function randomToken(length) {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
  return Array.from({ length }, () => alphabet[Math.floor(Math.random() * alphabet.length)]).join('');
}

export function parseTemplate(html, context = {}) {
  const { baseAmount = 0, amount: recipientAmount, order_id: orderId, tfn = '', email = '', totalAmount, date = new Date() } = context;
  const senderEmail = context.sender_email ?? context.senderEmail ?? '';
  const amount = Number.isFinite(Number(recipientAmount)) && recipientAmount !== '' ? Number(recipientAmount) : (Number.isFinite(Number(baseAmount)) ? Number(baseAmount) : 0);
  const total = totalAmount == null ? amount * 1.1 : Number(totalAmount);
  const dateValue = date instanceof Date ? date : new Date(date);
  const formattedDate = Number.isNaN(dateValue.getTime())
    ? ''
    : `${String(dateValue.getDate()).padStart(2, '0')}/${dateValue.getMonth() + 1}/${dateValue.getFullYear()}`;
  const values = {
    ...context,
    username: String(context.username ?? '').trim() ? context.username : usernameFromEmail(email),
    sender_username: String(context.sender_username ?? '').trim() ? context.sender_username : usernameFromEmail(senderEmail),
    order_id: orderId || randomToken(8),
    transaction_id: randomToken(12),
    customer_id: context.customer_id || randomToken(10),
    amount: amount.toFixed(2),
    total_amount: (Number.isFinite(total) ? total : amount).toFixed(2),
    tfn: String(tfn),
    email: String(email),
    eaddress: email ? `@${String(email).split('@')[0]}` : '',
    digi_no: randomToken(10),
    date: formattedDate,
    invoice_no: `INV-${randomToken(8)}`,
    number6: String(Math.floor(100000 + Math.random() * 900000)),
  };
  return String(html || '').replace(/\$([a-zA-Z_][a-zA-Z0-9_]*)(\b)/g, (_match, rawKey) => {
    let key = rawKey;
    let suffix = '';
    while (key.endsWith('_') && values[key] == null && !/^number\d+$/.test(key)) {
      key = key.slice(0, -1);
      suffix = `_${suffix}`;
    }
    if (key.startsWith('number')) {
      if (context[key] != null && String(context[key]) !== '') return `${String(context[key])}${suffix}`;
      const digits = Number(key.slice(6));
      if (!Number.isInteger(digits) || digits < 1 || digits > 12) return _match;
      return `${Math.floor(10 ** (digits - 1) + Math.random() * (9 * 10 ** (digits - 1)))}${suffix}`;
    }
    return values[key] == null ? _match : `${String(values[key])}${suffix}`;
  });
}

export function isGeneratedToken(value) {
  return TOKEN.test(value);
}
