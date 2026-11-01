const GIVEN_NAME_PREFIXES = `
  aaliyah aamir aaron abbas abdul abdullah abdulrahman adil adnan ahmed aisha akash alif ali alina amir amna anika anisa anwar arif arman arafat asif ashik atik ayesha aziz
  benjamin bilal brad brian
  carlos chris christian daniel david deepak dipu emily fatima farhan fahim faisal fatima frank
  gabriel george hasan hassan hossain ibrahim imran ismail jack james jane jason john jose joseph joy julia karim kevin khalid laura leo liam lily lucas mahmud maria mark mary matthew michael mizan mohammad mohammed monir
  nafis nancy nathan nicolas nikhil nora nusrat oliver omar osman patrick paul priya rachel rahim rakib reza robert saad sadaf saiful samuel sara sarah shafiq sohan steven tanvir tamim tasnim thomas tina waleed william yusuf zubair
` .trim().split(/\s+/).sort((left, right) => right.length - left.length);

const NON_NAME_PARTS = new Set(`
  account accounts admin app bd blog business c cse contact co company com dev developer edu email facebook finance git gmail help hr info instagram job jobs linkedin mail marketing me official online personal photo photography private sales service site student support team tech technology university web work www x
`.trim().split(/\s+/));

const PREFIX_PARTS = new Set(['dr', 'miss', 'mr', 'mrs', 'ms', 'prof']);

function titleCase(word) {
  const lower = word.toLocaleLowerCase('en');
  return lower.replace(/^(\p{L})/u, (letter) => letter.toLocaleUpperCase('en'));
}

function splitJoinedName(word) {
  if (GIVEN_NAME_PREFIXES.includes(word)) return [word];
  const prefix = GIVEN_NAME_PREFIXES.find((name) => word.startsWith(name) && word.length - name.length >= 3);
  return prefix ? [prefix, word.slice(prefix.length)] : [word];
}

/** Derive a readable first/last name from an email address local part. */
export function usernameFromEmail(email) {
  let address = String(email ?? '').trim();
  const angleAddress = address.match(/<([^<>]+)>/);
  if (angleAddress) address = angleAddress[1].trim();
  const atIndex = address.lastIndexOf('@');
  const localPart = (atIndex >= 0 ? address.slice(0, atIndex) : address).split('+', 1)[0];
  if (!localPart) return '';

  const separated = localPart
    .replace(/([a-z])([A-Z])/g, '$1 $2')
    .replace(/([A-Z])([A-Z][a-z])/g, '$1 $2')
    .normalize('NFKD')
    .replace(/\p{M}/gu, '')
    .replace(/\d+/g, ' ')
    .split(/[^\p{L}]+/u)
    .map((part) => part.toLocaleLowerCase('en'))
    .filter(Boolean);

  while (separated.length && PREFIX_PARTS.has(separated[0])) separated.shift();
  while (separated.length && NON_NAME_PARTS.has(separated[separated.length - 1])) separated.pop();

  const parts = separated
    .filter((part) => !NON_NAME_PARTS.has(part))
    .flatMap(splitJoinedName)
    .filter((part) => part.length > 0)
    .slice(0, 3);

  return parts.map(titleCase).join(' ');
}
