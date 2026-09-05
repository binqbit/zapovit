# Zapovit

Zapovit is a digital inheritance service for passing cryptocurrency assets, passwords, documents, and other important information to trusted recipients when the owner can no longer manage them.

It combines periodic activity confirmations, approval from trusted contacts, and predefined rules for releasing access.

## How it works

1. **Set up a plan.** The owner selects recipients, assigns information or assets, and configures inactivity periods and approval requirements.
2. **Choose trusted contacts.** Guardians help verify the circumstances and authorize activation. A plan can require a quorum, such as three out of five approvals.
3. **Confirm activity.** The owner responds to Telegram reminders and periodically completes a stronger authentication check in the application.
4. **Review inactivity.** Missed confirmations trigger additional reminders, a grace period, and requests for guardians to review the situation.
5. **Release access.** Once the required approvals and waiting period are complete, recipients can access their assigned information or claim assets. The owner can cancel the process before release.

Inactivity starts the verification process; it is not treated as proof of death.

## Transfer mechanisms

### Encrypted information

Sensitive information is organized into encrypted packages for designated recipients. Encrypted copies can be shared in advance, with the key material needed to unlock them released through the recovery process.

Approval rules determine when release is authorized. Cryptographic key sharing can additionally require several independent shares to reconstruct a key.

### Cryptocurrency assets

Wallet recovery instructions can be delivered as encrypted information. For predefined asset allocations, a smart wallet or contract records beneficiaries and their shares.

The owner periodically submits an authenticated heartbeat. When the configured inactivity, approval, and waiting conditions are met, beneficiaries can claim their allocation from assets controlled by the wallet or contract.

## Owner control

The owner manages recipients, guardians, check-in schedules, and release conditions. Guardians authorize the process, while recipients receive the information or assets assigned to them; these roles can belong to different people.
