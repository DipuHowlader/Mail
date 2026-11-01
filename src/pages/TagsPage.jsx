import React from 'react';

const TAG_GROUPS = [
  {
    title: 'Recipient',
    tags: [
      ['$email', 'Recipient email address.'],
      ['$username', 'Recipient name, or a readable name taken from the email address.'],
      ['$eaddress', 'The part of the recipient email before @, shown with @, for example @alex.'],
    ],
  },
  {
    title: 'Sender details',
    tags: [
      ['$sender_email', 'Email address of the sending account.'],
      ['$sender_username', 'Name taken from the sending account email address.'],
      ['$company_name', 'The Company Name setting.'],
      ['$companyName', 'The company name, using the other spelling.'],
      ['$baseAmount', 'The Base Amount setting.'],
      ['$tfn', 'The Support Phone / TFN setting.'],
      ['$due_date', 'A date 30 days after the campaign starts.'],
    ],
  },
  {
    title: 'Dates, amounts, and identifiers',
    tags: [
      ['$date', 'The current date.'],
      ['$amount', 'The amount for the recipient, when it is in your imported data.'],
      ['$total_amount', 'The total amount for the recipient, when it is in your imported data.'],
      ['$order_id', 'The order ID from your data, or a made-up one when it is missing.'],
      ['$transaction_id', 'The transaction ID from your data, or a made-up one when it is missing.'],
      ['$customer_id', 'The customer ID from your data, or a made-up one when it is missing.'],
      ['$invoice_no', 'The invoice number from your data, or a made-up one when it is missing.'],
      ['$digi_no', 'A made-up 10-digit number.'],
      ['$number4', 'A made-up number with the number of digits you choose. Use $number1 through $number12.'],
    ],
  },
];

const FILTERS = [
  ['upper', 'Convert text to capital letters.'],
  ['lower', 'Convert text to small letters.'],
  ['capitalize', 'Capitalize the first letter.'],
  ['trim', 'Remove spaces at the start and end.'],
  ['currency', 'Format a value as currency.'],
  ['number', 'Format a number with a chosen number of decimal places.'],
  ['round', 'Round a number.'],
  ['date', 'Format a date.'],
  ['default', 'Use a backup value when the field is empty.'],
  ['json', 'Format a value as plain data.'],
  ['safe', 'Allow HTML code. Use only with content you trust.'],
];

export default function TagsPage({ onClose, onCopy }) {
  return (
    <div
      className="fixed inset-0 z-[70] flex items-center justify-center bg-black/70 p-4"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <section
        className="flex max-h-[92vh] w-full max-w-4xl flex-col overflow-hidden rounded-xl border border-line bg-canvas-elevated shadow-panel"
        role="dialog"
        aria-modal="true"
        aria-labelledby="tags-page-title"
      >
        <header className="flex shrink-0 items-center justify-between border-b border-line px-5 py-4">
          <div>
            <h2 id="tags-page-title" className="text-sm font-semibold text-ink">Tags reference</h2>
            <p className="mt-1 text-xs text-ink-muted">Use these tags in your subject, HTML code, and file names. Click a tag to copy it.</p>
          </div>
          <button type="button" onClick={onClose} aria-label="Close tags reference" className="rounded p-1.5 text-ink-muted hover:bg-canvas-secondary hover:text-ink">×</button>
        </header>

        <div className="min-h-0 space-y-5 overflow-y-auto p-5">
          {TAG_GROUPS.map((group) => (
            <section key={group.title}>
              <h3 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-ink-muted">{group.title}</h3>
              <div className="grid gap-2 md:grid-cols-2">
                {group.tags.map(([tag, description]) => (
                  <button
                    key={tag}
                    type="button"
                    onClick={() => onCopy(tag)}
                    className="flex min-w-0 items-start gap-3 rounded-lg border border-line bg-canvas-secondary p-3 text-left hover:border-line-accent"
                  >
                    <code className="shrink-0 rounded bg-canvas-elevated px-2 py-1 text-xs text-accent-soft">{tag}</code>
                    <span className="text-xs leading-relaxed text-ink-secondary">{description}</span>
                  </button>
                ))}
              </div>
            </section>
          ))}

          <section className="rounded-lg border border-line bg-canvas-secondary p-4">
            <h3 className="text-[11px] font-semibold uppercase tracking-wide text-ink-muted">Imported columns</h3>
            <p className="mt-2 text-xs leading-relaxed text-ink-secondary">
              Column names from your file become tags. For example, a <code>first_name</code> column becomes <code>$first_name</code>. Capital letters matter, so write the name exactly as it appears. Columns such as <code>amount</code>, <code>order_id</code>, or <code>customer_id</code> also work.
            </p>
          </section>

          <section className="rounded-lg border border-line bg-canvas-secondary p-4">
            <h3 className="text-[11px] font-semibold uppercase tracking-wide text-ink-muted">How to write tags</h3>
            <div className="mt-3 grid gap-3 md:grid-cols-2">
              <div className="rounded-md bg-canvas-elevated p-3 text-xs leading-relaxed text-ink-secondary">
                <code className="text-accent-soft">{'{{amount | currency:\'USD\'}}'}</code>
                <p className="mt-1">Double braces run small expressions and keep inserted text safe. Tags such as <code>$email</code> are a short way to insert simple values.</p>
              </div>
              <div className="rounded-md bg-canvas-elevated p-3 text-xs leading-relaxed text-ink-secondary">
                <code className="text-accent-soft">{'$order.id  ·  ${amount * 1.1}'}</code>
                <p className="mt-1">Use dots to reach values inside other values, and <code>${'{expression}'}</code> for a full expression.</p>
              </div>
              <div className="rounded-md bg-canvas-elevated p-3 text-xs leading-relaxed text-ink-secondary">
                <code className="text-accent-soft">{'{{#if amount}}...{{else}}...{{/if}}'}</code>
                <p className="mt-1">Use <code>if</code> blocks to show content only when needed, and <code>each</code> blocks to repeat content for each item in a list.</p>
              </div>
              <div className="rounded-md bg-canvas-elevated p-3 text-xs leading-relaxed text-ink-secondary">
                <code className="text-accent-soft">{'{{#each items as item}}...{{/each}}'}</code>
                <p className="mt-1">Inside a loop, use the name (here <code>item</code>), <code>@index</code>, <code>@first</code>, and <code>@last</code>.</p>
              </div>
              <div className="rounded-md bg-canvas-elevated p-3 text-xs leading-relaxed text-ink-secondary">
                <code className="text-accent-soft">{'{{{trusted_html}}}'}</code>
                <p className="mt-1">Triple braces insert HTML code. Only use this with content you trust.</p>
              </div>
            </div>
            <div className="mt-4">
              <h4 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-ink-muted">Filters</h4>
              <div className="grid gap-2 md:grid-cols-2">
                {FILTERS.map(([name, description]) => (
                  <div key={name} className="flex items-start gap-2 rounded-md bg-canvas-elevated p-2 text-xs">
                    <code className="shrink-0 text-accent-soft">{name}</code>
                    <span className="text-ink-secondary">{description}</span>
                  </div>
                ))}
              </div>
            </div>
          </section>
        </div>

        <footer className="flex shrink-0 justify-end border-t border-line px-5 py-3">
          <button type="button" onClick={onClose} className="rounded-lg bg-accent px-4 py-2 text-xs font-semibold text-white hover:bg-accent-soft">Done</button>
        </footer>
      </section>
    </div>
  );
}
