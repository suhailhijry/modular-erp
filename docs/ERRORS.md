# Error codes

Every `code` this API can answer with. **Branch on the code, never on
`detail`** — the detail is prose in whichever language the request asked
for, and it changes; the code does not.

Every error is `application/problem+json` (RFC 9457) with `code`,
`detail`, and `args` carrying the values the message names. `args` is
where a client gets the machine-readable version of what went wrong —
which account, which module, how much was outstanding.

<!-- Generated from the message catalog by `crates/spa-api/tests/errors.rs`.
Run `just errors` after adding a code; CI fails if this drifts. -->


## `access`

### `access.denied`

- **en** — You do not have access to this workspace.
- **ar** — ليس لديك صلاحية الوصول إلى مساحة العمل هذه.

### `access.identity_suspended`

- **en** — This account has been suspended. Contact your administrator.
- **ar** — تم تعليق هذا الحساب. يُرجى التواصل مع المسؤول.

### `access.no_such_identity`

- **en** — We could not sign you in. Please sign in again.
- **ar** — تعذّر تسجيل دخولك. يُرجى تسجيل الدخول مرة أخرى.

### `access.not_permitted`

- **en** — Your role does not allow this ({capability}). Ask someone with permission.
- **ar** — دورك لا يسمح بهذا الإجراء ({capability}). يُرجى طلبه ممن لديه الصلاحية.

### `access.tenant_provisioning`

- **en** — Your workspace is still being set up. This usually takes a few seconds.
- **ar** — لا يزال إعداد مساحة العمل جارياً. عادةً ما يستغرق ذلك بضع ثوانٍ.

### `access.tenant_unavailable`

- **en** — This workspace is unavailable. Contact your administrator.
- **ar** — مساحة العمل هذه غير متاحة. يُرجى التواصل مع المسؤول.


## `auth`

### `auth.handle_taken`

- **en** — {handle} already has an account. Sign in with it instead.
- **ar** — لدى {handle} حساب بالفعل. سجّل الدخول به بدلًا من ذلك.

### `auth.invalid_credentials`

- **en** — Those sign-in details are not correct. Please try again.
- **ar** — بيانات تسجيل الدخول غير صحيحة. يُرجى المحاولة مرة أخرى.

### `auth.session_expired`

- **en** — Your session has ended. Please sign in again.
- **ar** — انتهت جلستك. يُرجى تسجيل الدخول مرة أخرى.


## `eventlog`

### `eventlog.concurrent_modification`

- **en** — Someone else changed this while you were working on it. Please review and try again.
- **ar** — قام شخص آخر بتعديل هذا أثناء عملك عليه. يُرجى المراجعة والمحاولة مرة أخرى.

### `eventlog.internal_error`

- **en** — Something went wrong on our side. The problem has been recorded.
- **ar** — حدث خطأ لدينا. تم تسجيل المشكلة.


## `invitations`

### `invitations.not_valid`

- **en** — That invitation is no longer valid. Ask whoever invited you for a new link.
- **ar** — لم تعد هذه الدعوة صالحة. اطلب رابطًا جديدًا ممن دعاك.


## `ledger`

### `ledger.account_closed`

- **en** — Account {code} is closed and cannot take new entries.
- **ar** — الحساب {code} مغلق ولا يقبل قيودًا جديدة.

### `ledger.account_exists`

- **en** — Account {code} already exists.
- **ar** — الحساب {code} موجود بالفعل.

### `ledger.already_posted`

- **en** — This entry has already been posted.
- **ar** — تم ترحيل هذا القيد بالفعل.

### `ledger.already_reversed`

- **en** — That entry was already reversed by {by}.
- **ar** — تم عكس هذا القيد بالفعل بواسطة {by}.

### `ledger.amount_out_of_range`

- **en** — That amount is too large to record.
- **ar** — هذا المبلغ أكبر من أن يُسجَّل.

### `ledger.does_not_balance`

- **en** — Debits and credits differ by {difference}.
- **ar** — يوجد فرق بين المدين والدائن مقداره {difference}.

### `ledger.mixed_currencies`

- **en** — This entry is in {expected}, but a line is in {found}.
- **ar** — هذا القيد بعملة {expected}، لكن أحد السطور بعملة {found}.

### `ledger.no_such_account`

- **en** — There is no account {code}.
- **ar** — لا يوجد حساب {code}.

### `ledger.no_such_entry`

- **en** — There is no entry {entry}.
- **ar** — لا يوجد قيد {entry}.

### `ledger.period_closed`

- **en** — The books are closed before {closed_before}, and this is dated {on}.              Post the correction in the period that is open.
- **ar** — أُقفلت الدفاتر قبل {closed_before}، وتاريخ هذا القيد {on}.              سجّل التصحيح في الفترة المفتوحة.

### `ledger.too_few_lines`

- **en** — An entry needs at least two lines; this has {n}.
- **ar** — يحتاج القيد إلى سطرين على الأقل، والموجود {n} سطر.

### `ledger.zero_line`

- **en** — A line cannot be for zero.
- **ar** — لا يمكن أن يكون السطر بقيمة صفر.


## `members`

### `members.already_a_member`

- **en** — {handle} already has access. Change their role instead.
- **ar** — {handle} لديه صلاحية الوصول بالفعل. يمكنك تغيير دوره بدلاً من ذلك.

### `members.last_owner`

- **en** — A workspace must keep at least one owner. Make someone else an owner first.
- **ar** — يجب أن يبقى للمساحة مالك واحد على الأقل. عيّن مالكًا آخر أولاً.

### `members.not_a_member`

- **en** — That person is not a member of this tenant.
- **ar** — هذا الشخص ليس عضوًا لدى هذا المستأجر.


## `ops`

### `ops.clusters_at_limit`

- **en** — {n} clusters are at their limit.
- **ar** — {n} مجموعة بلغت حدّها الأقصى.


## `provisioning`

### `provisioning.no_capacity`

- **en** — We could not create your workspace right now. Please try again in a few minutes.
- **ar** — تعذّر إنشاء مساحة العمل الآن. يُرجى المحاولة بعد بضع دقائق.

### `provisioning.slug_taken`

- **en** — The name {slug} is already taken. Please choose another.
- **ar** — الاسم {slug} مستخدم بالفعل. يُرجى اختيار اسم آخر.


## `purchases`

### `purchases.invalid_reference`

- **en** — {reference} cannot be used as a reference. Use letters, digits, and . - _ only.
- **ar** — لا يمكن استخدام {reference} كمرجع. استخدم الحروف والأرقام و. - _ فقط.

### `purchases.mixed_currencies`

- **en** — Every line of a bill must be in the same currency.
- **ar** — يجب أن تكون جميع سطور الفاتورة بالعملة نفسها.

### `purchases.negative_tax`

- **en** — VAT on a bill cannot be negative.
- **ar** — لا يمكن أن تكون ضريبة القيمة المضافة على الفاتورة سالبة.

### `purchases.no_supplier_vat_number`

- **en** — Input VAT can only be reclaimed against a registered supplier. Add their VAT number, or record the bill without tax.
- **ar** — لا يمكن استرداد ضريبة المدخلات إلا من مورّد مسجَّل. أضف رقمه الضريبي، أو سجّل الفاتورة دون ضريبة.

### `purchases.not_a_payment`

- **en** — A payment must be a positive amount.
- **ar** — يجب أن تكون قيمة الدفعة موجبة.

### `purchases.not_recorded`

- **en** — Bill {bill} has not been recorded.
- **ar** — لم تُسجَّل فاتورة المورّد {bill}.

### `purchases.nothing_on_it`

- **en** — A bill needs at least one line that comes to something.
- **ar** — تحتاج الفاتورة إلى سطر واحد على الأقل بقيمة غير صفرية.

### `purchases.overpayment`

- **en** — Only {outstanding} is outstanding, and the payment is {offered}.
- **ar** — المتبقي هو {outstanding} فقط، ومبلغ الدفعة {offered}.

### `purchases.payment_currency`

- **en** — This bill is in {expected}, but the payment is in {found}.
- **ar** — هذه الفاتورة بعملة {expected}، لكن الدفعة بعملة {found}.

### `purchases.tax_on_an_untaxed_line`

- **en** — A {category} line carries no VAT, and this one is charged {tax}. Check the supplier's invoice.
- **ar** — لا تحمل السطور من نوع {category} ضريبة، وهذا السطر عليه {tax}. راجع فاتورة المورّد.


## `request`

### `request.empty_period`

- **en** — A period must end after it starts. `until` is exclusive.
- **ar** — يجب أن تنتهي الفترة بعد بدايتها. تاريخ الانتهاء غير شامل.

### `request.invalid_id`

- **en** — {id} cannot be used as an identifier.
- **ar** — لا يمكن استخدام {id} كمعرّف.

### `request.invalid_query`

- **en** — The query string could not be read: {reason}
- **ar** — تعذّرت قراءة معطيات الاستعلام: {reason}

### `request.malformed_body`

- **en** — The request body could not be read: {reason}
- **ar** — تعذّرت قراءة محتوى الطلب: {reason}

### `request.module_deprecated`

- **en** — The {module} module is no longer offered: {why}. Tenants already using it keep it.
- **ar** — لم تعد وحدة {module} متاحة: {why}. تحتفظ الجهات التي تستخدمها بها.

### `request.module_in_use`

- **en** — {dependent} needs {module}. Turn {dependent} off first.
- **ar** — تحتاج وحدة {dependent} إلى {module}. أوقف {dependent} أولًا.

### `request.module_not_enabled`

- **en** — The {module} module is not enabled for this tenant.
- **ar** — وحدة {module} غير مفعَّلة لدى هذا المستأجر.

### `request.module_requires`

- **en** — The {module} module needs {required}. Add it to the list.
- **ar** — تحتاج وحدة {module} إلى {required}. أضفها إلى القائمة.

### `request.no_such_bill`

- **en** — There is no bill {bill}.
- **ar** — لا توجد فاتورة مورّد {bill}.

### `request.no_such_invoice`

- **en** — There is no invoice {invoice}.
- **ar** — لا توجد فاتورة {invoice}.

### `request.not_caught_up`

- **en** — Still catching up ({behind} to go). Please try again in a moment.
- **ar** — لا يزال التحديث جاريًا (متبقٍ {behind}). يُرجى المحاولة بعد لحظات.

### `request.password_too_short`

- **en** — A password needs at least {n} characters.
- **ar** — تحتاج كلمة المرور إلى {n} حرف على الأقل.

### `request.unknown_account_kind`

- **en** — {kind} is not an account type. Use asset, liability, equity, revenue or expense.
- **ar** — {kind} ليس نوع حساب. استخدم أصل أو التزام أو حقوق ملكية أو إيراد أو مصروف.

### `request.unknown_chart`

- **en** — There is no chart of accounts called {chart}.
- **ar** — لا يوجد دليل حسابات باسم {chart}.

### `request.unknown_currency`

- **en** — {currency} is not a currency code. Use three letters, like SAR.
- **ar** — {currency} ليس رمز عملة. استخدم ثلاثة أحرف، مثل SAR.

### `request.unknown_module`

- **en** — There is no module called {module}.
- **ar** — لا توجد وحدة باسم {module}.

### `request.unknown_role`

- **en** — {role} is not a role. Use owner, accountant, clerk or viewer.
- **ar** — {role} ليس دورًا. استخدم owner أو accountant أو clerk أو viewer.

### `request.unknown_vat_category`

- **en** — {vat} is not a VAT treatment. Use standard, zero or exempt.
- **ar** — {vat} ليست معاملة ضريبية. استخدم standard أو zero أو exempt.

### `request.unsupported_media_type`

- **en** — This endpoint takes `Content-Type: application/json`.
- **ar** — يقبل هذا المسار `Content-Type: application/json` فقط.


## `sales`

### `sales.already_cancelled`

- **en** — That invoice was already cancelled by credit note {by}.
- **ar** — تم إلغاء هذه الفاتورة بالفعل بإشعار دائن {by}.

### `sales.amount_out_of_range`

- **en** — That amount is too large to record.
- **ar** — هذا المبلغ أكبر من أن يُسجَّل.

### `sales.has_payments`

- **en** — Invoice {invoice} has payments against it. Refund them before crediting it.
- **ar** — توجد دفعات على الفاتورة {invoice}. أعِد المبالغ قبل إصدار إشعار دائن.

### `sales.invalid_reference`

- **en** — {reference} cannot be used as a reference. Use letters, digits, and . - _ only.
- **ar** — لا يمكن استخدام {reference} كمرجع. استخدم الحروف والأرقام و. - _ فقط.

### `sales.mixed_currencies`

- **en** — Every line of an invoice must be in the same currency.
- **ar** — يجب أن تكون جميع سطور الفاتورة بالعملة نفسها.

### `sales.not_a_payment`

- **en** — A payment must be a positive amount.
- **ar** — يجب أن تكون قيمة الدفعة موجبة.

### `sales.not_issued`

- **en** — Invoice {invoice} has not been issued.
- **ar** — لم تُصدَر الفاتورة {invoice}.

### `sales.nothing_to_invoice`

- **en** — An invoice needs at least one line that comes to something.
- **ar** — تحتاج الفاتورة إلى سطر واحد على الأقل بقيمة غير صفرية.

### `sales.overpayment`

- **en** — Only {outstanding} is outstanding, and the payment is {offered}.
- **ar** — المتبقي هو {outstanding} فقط، ومبلغ الدفعة {offered}.

### `sales.payment_currency`

- **en** — This invoice is in {expected}, but the payment is in {found}.
- **ar** — هذه الفاتورة بعملة {expected}، لكن الدفعة بعملة {found}.


## `system`

### `system.internal_error`

- **en** — Something went wrong on our side. The problem has been recorded.
- **ar** — حدث خطأ لدينا. تم تسجيل المشكلة.

### `system.overloaded`

- **en** — The system is busy right now. Please try again in a moment.
- **ar** — النظام مشغول حالياً. يُرجى المحاولة مرة أخرى بعد قليل.

