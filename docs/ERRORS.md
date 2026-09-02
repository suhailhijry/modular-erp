# Error codes

Every `code` this API can answer with. **Branch on the code, never on
`detail`** — the detail is prose in whichever language the request asked
for, and it changes; the code does not.

Every error is `application/problem+json` (RFC 9457) with `code`,
`detail`, and `args` carrying the values the message names. `args` is
where a client gets the machine-readable version of what went wrong —
which account, which module, how much was outstanding.

<!-- Generated from the message catalog by `crates/erp-api/tests/errors.rs`.
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


## `booking`

### `booking.allowance_too_large`

- **en** — A discount cannot be larger than what it comes off.
- **ar** — لا يمكن أن يتجاوز الخصم المبلغ المحسوم منه.

### `booking.amount_out_of_range`

- **en** — That amount is too large to record.
- **ar** — هذا المبلغ أكبر من أن يُسجَّل.

### `booking.cannot_move`

- **en** — A booking cannot go from {from} to {to}.
- **ar** — لا يمكن نقل الحجز من {from} إلى {to}.

### `booking.invalid_reference`

- **en** — {reference} cannot be used as a reference.
- **ar** — لا يمكن استخدام {reference} كمرجع.

### `booking.may_not_work`

- **en** — {id} may not be rostered: a work document has lapsed, or they have left.
- **ar** — {id} لا يمكن إسناده: انتهت صلاحية وثيقة عمل، أو لم يعد على رأس العمل.

### `booking.mixed_currencies`

- **en** — Every amount on a booking must be in the same currency.
- **ar** — يجب أن تكون جميع المبالغ في الحجز بالعملة نفسها.

### `booking.no_name`

- **en** — A booking needs a name to put in the diary.
- **ar** — يحتاج الحجز إلى اسم يظهر في المفكرة.

### `booking.no_such_branch`

- **en** — There is no open branch {branch}. A resource can only be placed at a branch that exists and is still trading.
- **ar** — لا يوجد فرع مفتوح {branch}. المورد لا يُسنَد إلا لفرع قائم وما زال يعمل.

### `booking.no_such_customer`

- **en** — There is no customer {customer}.
- **ar** — لا يوجد عميل {customer}.

### `booking.no_such_employee`

- **en** — There is no employee {id}.
- **ar** — لا يوجد موظف {id}.

### `booking.no_such_line`

- **en** — This booking has no line {line}.
- **ar** — لا يحتوي هذا الحجز على البند {line}.

### `booking.no_such_reservation`

- **en** — There is no booking {reservation}.
- **ar** — لا يوجد حجز {reservation}.

### `booking.no_such_resource`

- **en** — There is nothing bookable called {resource}.
- **ar** — لا يوجد شيء قابل للحجز باسم {resource}.

### `booking.no_such_trade`

- **en** — There is no ready-made rota called {trade}.
- **ar** — لا توجد قائمة موارد جاهزة باسم {trade}.

### `booking.not_a_rate`

- **en** — A price cannot be negative.
- **ar** — لا يمكن أن يكون السعر بالسالب.

### `booking.not_an_allowance`

- **en** — A discount is the amount taken off, so it is a positive number.
- **ar** — الخصم هو المبلغ المحسوم، لذا يجب أن يكون رقمًا موجبًا.

### `booking.not_offered`

- **en** — {resource} is not open at that time.
- **ar** — {resource} غير متاح في ذلك الوقت.

### `booking.nothing_charged`

- **en** — A priced line must be for at least one.
- **ar** — يجب أن يكون البند المسعّر لواحد على الأقل.

### `booking.nothing_to_book`

- **en** — A booking needs at least one thing being booked.
- **ar** — يحتاج الحجز إلى خدمة واحدة على الأقل.

### `booking.over`

- **en** — This booking is already {stage}, and nothing more can happen to it.
- **ar** — هذا الحجز {stage} بالفعل، ولا يمكن تغييره بعد ذلك.

### `booking.reserved_name`

- **en** — Names beginning with "customer." are kept for customers' own diaries.
- **ar** — الأسماء التي تبدأ بـ "customer." محجوزة لمفكرات العملاء.

### `booking.resource_has_no_name`

- **en** — Give it a name people will recognise on the calendar.
- **ar** — امنحه اسمًا يتعرف عليه الناس في التقويم.

### `booking.unknown_kind`

- **en** — {value} is not a person, a place or a thing.
- **ar** — {value} ليس شخصًا ولا مكانًا ولا شيئًا.

### `booking.unknown_stage`

- **en** — {value} is not a stage a booking can be in.
- **ar** — {value} ليست مرحلة يمكن أن يكون الحجز فيها.

### `booking.withdrawn`

- **en** — {resource} is out of service.
- **ar** — {resource} خارج الخدمة.


## `branches`

### `branches.closed`

- **en** — Branch {id} is closed and takes no new documents. Its old ones are unaffected.
- **ar** — الفرع {id} مغلق ولا يستقبل مستندات جديدة. مستنداته السابقة كما هي.

### `branches.no_address`

- **en** — A branch needs a street and a city.
- **ar** — الفرع يحتاج إلى شارع ومدينة.

### `branches.no_name`

- **en** — A branch needs a name.
- **ar** — الفرع يحتاج إلى اسم.

### `branches.no_such_branch`

- **en** — There is no branch {id}.
- **ar** — لا يوجد فرع {id}.

### `branches.not_a_country`

- **en** — {country} is not a two-letter ISO 3166-1 country code.
- **ar** — {country} ليس رمز دولة من حرفين وفق ISO 3166-1.


## `crm`

### `crm.archived`

- **en** — Customer {customer} is archived. Restore them first.
- **ar** — العميل {customer} مؤرشف. استعده أولًا.

### `crm.name_too_long`

- **en** — A name may not be longer than {n} characters.
- **ar** — لا يمكن أن يتجاوز الاسم {n} حرف.

### `crm.no_contact`

- **en** — A customer needs a phone number or an email address.
- **ar** — يحتاج العميل إلى رقم جوال أو بريد إلكتروني.

### `crm.no_name`

- **en** — A customer needs a name.
- **ar** — يحتاج العميل إلى اسم.

### `crm.no_such_customer`

- **en** — There is no customer {customer}.
- **ar** — لا يوجد عميل {customer}.

### `crm.not_a_vat_number`

- **en** — {value} is not a Saudi VAT number. It is fifteen digits beginning and ending with 3.
- **ar** — {value} ليس رقم تسجيل ضريبي سعودي. يتكون من خمسة عشر رقمًا يبدأ وينتهي بالرقم ٣.

### `crm.person_with_vat_number`

- **en** — A person does not hold a VAT registration. Record them as a company.
- **ar** — الفرد لا يملك تسجيلًا ضريبيًا. سجّله كمنشأة.

### `crm.unknown_kind`

- **en** — A customer is a person or a company.
- **ar** — العميل إما فرد أو منشأة.


## `eventlog`

### `eventlog.already_exists`

- **en** — Something else already exists under that identifier. This is not the same request that created it, so it has not been saved.
- **ar** — يوجد شيء آخر بالفعل تحت هذا المعرّف. هذا ليس نفس الطلب الذي أنشأه، فلم يتم الحفظ.

### `eventlog.concurrent_modification`

- **en** — Someone else changed this while you were working on it. Please review and try again.
- **ar** — قام شخص آخر بتعديل هذا أثناء عملك عليه. يُرجى المراجعة والمحاولة مرة أخرى.

### `eventlog.internal_error`

- **en** — Something went wrong on our side. The problem has been recorded.
- **ar** — حدث خطأ لدينا. تم تسجيل المشكلة.


## `hr`

### `hr.backwards_leave`

- **en** — Leave cannot end before it starts.
- **ar** — لا يمكن أن تنتهي الإجازة قبل أن تبدأ.

### `hr.cycle`

- **en** — {id} cannot report to somebody in their own team.
- **ar** — {id} لا يمكن أن يكون تابعًا لأحد أفراد فريقه.

### `hr.database`

- **en** — The org chart could not be read. Try again.
- **ar** — تعذّرت قراءة الهيكل التنظيمي. أعد المحاولة.

### `hr.deductions_exceed_pay`

- **en** — What is taken off comes to more than what is paid.
- **ar** — مجموع الاستقطاعات يتجاوز المستحق.

### `hr.left`

- **en** — Employee {id} has left.
- **ar** — الموظف {id} لم يعد على رأس العمل.

### `hr.no_contact`

- **en** — An employee needs a phone number or an email address.
- **ar** — الموظف يحتاج إلى رقم هاتف أو بريد إلكتروني.

### `hr.no_document_number`

- **en** — A document needs its number.
- **ar** — الوثيقة تحتاج إلى رقمها.

### `hr.no_name`

- **en** — An employee needs a name.
- **ar** — الموظف يحتاج إلى اسم.

### `hr.no_such_branch`

- **en** — There is no open branch {branch}.
- **ar** — لا يوجد فرع مفتوح {branch}.

### `hr.no_such_employee`

- **en** — There is no employee {id}.
- **ar** — لا يوجد موظف {id}.

### `hr.no_such_manager`

- **en** — There is no employee {id} to report to.
- **ar** — لا يوجد موظف {id} ليكون مسؤولًا.

### `hr.not_a_claim`

- **en** — {claim} is not usable as a permission name.
- **ar** — {claim} غير صالح كاسم صلاحية.

### `hr.not_a_day_of_work`

- **en** — A day has 1440 minutes in it.
- **ar** — اليوم ١٤٤٠ دقيقة.

### `hr.not_a_salary`

- **en** — A salary needs positive basic pay, and every part in one currency.
- **ar** — الراتب يحتاج إلى أساسي موجب، وكل بند بعملة واحدة.

### `hr.unknown_document`

- **en** — {kind} is not a kind of document this system tracks.
- **ar** — {kind} ليس نوع وثيقة يتتبعه هذا النظام.

### `hr.unknown_leave`

- **en** — {kind} is not a kind of leave. Use annual, sick, unpaid or statutory.
- **ar** — {kind} ليس نوع إجازة. استخدم annual أو sick أو unpaid أو statutory.


## `hr_sa`

### `hr_sa.amount_out_of_range`

- **en** — That amount is outside the range this system can hold.
- **ar** — المبلغ خارج النطاق الذي يستطيع النظام تمثيله.

### `hr_sa.database`

- **en** — That could not be read. Try again.
- **ar** — تعذّرت القراءة. أعد المحاولة.

### `hr_sa.no_salary`

- **en** — No salary is recorded for {id}.
- **ar** — لا يوجد راتب مسجَّل لـ {id}.

### `hr_sa.no_such_employee`

- **en** — There is no employee {id}.
- **ar** — لا يوجد موظف {id}.

### `hr_sa.not_left`

- **en** — {id} is still employed, so there is no end of service to compute.
- **ar** — {id} ما زال على رأس العمل، فلا توجد نهاية خدمة لاحتسابها.

### `hr_sa.unknown_footing`

- **en** — {footing} is not a GOSI footing. Use saudi or non_saudi.
- **ar** — {footing} ليس تصنيفًا في التأمينات. استخدم saudi أو non_saudi.

### `hr_sa.unknown_leaving`

- **en** — {reason} is not a reason for leaving. Use dismissed, resigned, in_full or for_cause.
- **ar** — {reason} ليس سببًا لانتهاء الخدمة. استخدم dismissed أو resigned أو in_full أو for_cause.


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

### `ledger.no_such_branch`

- **en** — There is no open branch {branch}. A document can only be dated to a branch that exists and is still trading.
- **ar** — لا يوجد فرع مفتوح {branch}. المستند لا يُنسب إلا لفرع قائم وما زال يعمل.

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


## `links`

### `links.already_used`

- **en** — That link has already been used. It only works once.
- **ar** — سبق استخدام هذا الرابط. وهو يعمل مرة واحدة فقط.

### `links.expired`

- **en** — That link has expired. Ask for a new one.
- **ar** — انتهت صلاحية هذا الرابط. اطلب رابطًا جديدًا.

### `links.no_such_link`

- **en** — That link does not go anywhere. Check it was copied whole.
- **ar** — هذا الرابط لا يؤدي إلى شيء. تأكد من نسخه كاملًا.

### `links.not_a_target`

- **en** — {target} is not somewhere a link may point.
- **ar** — {target} ليس موضعًا يمكن أن يشير إليه رابط.


## `mail`

### `mail.invitation_body`

- **en** — You have been invited to join {company}.

Open this link to accept and choose a password:
{link}

The link works once and expires. If you were not expecting this, ignore this message — nothing happens until you open it.
- **ar** — تمت دعوتك للانضمام إلى {company}.

افتح هذا الرابط لقبول الدعوة واختيار كلمة مرور:
{link}

يعمل الرابط مرة واحدة ثم ينتهي. إن لم تكن تتوقع هذه الرسالة فتجاهلها — لا يحدث شيء حتى تفتحها.

### `mail.invitation_subject`

- **en** — You have been invited to {company}
- **ar** — تمت دعوتك إلى {company}

### `mail.signup_body`

- **en** — Somebody asked to create {company} with this address.

Open this link to confirm it and finish setting up:
{link}

The link works once and expires within a day. Nothing has been created yet, so if this was not you, ignore this message and nothing will be.
- **ar** — طلب أحدهم إنشاء {company} بهذا البريد.

افتح هذا الرابط لتأكيده وإكمال الإعداد:
{link}

يعمل الرابط مرة واحدة وتنتهي صلاحيته خلال يوم. لم يُنشأ شيء بعد، فإن لم تكن أنت من طلب ذلك فتجاهل الرسالة ولن يُنشأ شيء.

### `mail.signup_subject`

- **en** — Confirm your address to create {company}
- **ar** — أكِّد بريدك لإنشاء {company}


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


## `messaging`

### `messaging.database`

- **en** — That could not be read. Try again.
- **ar** — تعذّرت القراءة. أعد المحاولة.

### `messaging.empty_template`

- **en** — A template needs something to say.
- **ar** — يحتاج القالب إلى نص.

### `messaging.missing_language`

- **en** — This template has no wording in {locale}.
- **ar** — لا توجد صياغة بلغة {locale} في هذا القالب.

### `messaging.needs_a_subject`

- **en** — A message on {channel} needs a subject line.
- **ar** — تحتاج الرسالة على {channel} إلى سطر موضوع.

### `messaging.negative_budget`

- **en** — A budget cannot be negative, and {limit} is.
- **ar** — لا يمكن أن تكون الميزانية سالبة، و{limit} كذلك.

### `messaging.no_subject_line`

- **en** — {channel} has no subject line, so remove it.
- **ar** — لا يوجد سطر موضوع في {channel}، فاحذفه.

### `messaging.no_such_template`

- **en** — There is no template called {name}, or it is switched off.
- **ar** — لا يوجد قالب باسم {name}، أو أنه متوقف.

### `messaging.not_a_month`

- **en** — {period} is not a month. Write it as 2026-05.
- **ar** — {period} ليس شهرًا. اكتبه هكذا: 2026-05.

### `messaging.not_a_name`

- **en** — {name} is not a template name. Use lower case, digits, dots and underscores.
- **ar** — {name} ليس اسم قالب. استخدم حروفًا صغيرة وأرقامًا ونقاطًا وشرطات سفلية.

### `messaging.over_budget`

- **en** — {channel} has used its whole budget of {limit} for this month.
- **ar** — استهلك {channel} كامل ميزانيته البالغة {limit} لهذا الشهر.

### `messaging.unknown_audience`

- **en** — {audience} is not an audience.
- **ar** — {audience} ليس جمهورًا.

### `messaging.unknown_binding`

- **en** — {binding} is not something a message about {topic} can say.
- **ar** — {binding} ليس مما يمكن أن تقوله رسالة عن {topic}.

### `messaging.unknown_channel`

- **en** — {channel} is not a channel. Use email, sms, push or whatsapp.
- **ar** — {channel} ليس قناة. استخدم email أو sms أو push أو whatsapp.

### `messaging.unknown_language`

- **en** — {language} is not a language this system speaks.
- **ar** — {language} ليست لغة يتحدثها هذا النظام.

### `messaging.unknown_platform`

- **en** — {platform} is not a platform. Use apns, fcm or web.
- **ar** — {platform} ليست منصة. استخدم apns أو fcm أو web.

### `messaging.unknown_topic`

- **en** — {topic} is not something a message can be about.
- **ar** — {topic} ليس موضوعًا يمكن أن تدور حوله رسالة.

### `messaging.unreachable`

- **en** — Nobody in {audience} can be reached on {channel}.
- **ar** — لا يمكن الوصول إلى {audience} عبر {channel}.

### `messaging.wrong_audience`

- **en** — A message about {topic} cannot be addressed to {audience}.
- **ar** — لا يمكن توجيه رسالة عن {topic} إلى {audience}.


## `occupancy`

### `occupancy.empty_span`

- **en** — A booking has to end after it starts.
- **ar** — يجب أن ينتهي الحجز بعد بدايته.

### `occupancy.internal_error`

- **en** — Something went wrong on our side. The problem has been recorded.
- **ar** — حدث خطأ لدينا. تم تسجيل المشكلة.

### `occupancy.no_such_resource`

- **en** — There is nothing here called {resource}.
- **ar** — لا يوجد مورد باسم {resource}.

### `occupancy.nothing_claimed`

- **en** — A booking has to be for at least one place.
- **ar** — يجب أن يكون الحجز لمكان واحد على الأقل.

### `occupancy.overbooked`

- **en** — {resource} is already holding {held} of {capacity} at that time, so {wanted} more will not fit.
- **ar** — {resource} محجوز بمقدار {held} من {capacity} في ذلك الوقت، ولا يتسع لـ {wanted} إضافية.

### `occupancy.span_too_long`

- **en** — A booking may not run longer than {n} days.
- **ar** — لا يمكن أن يمتد الحجز أكثر من {n} يوم.


## `ops`

### `ops.clusters_at_limit`

- **en** — {n} clusters are at their limit.
- **ar** — {n} مجموعة بلغت حدّها الأقصى.


## `payroll`

### `payroll.amount_out_of_range`

- **en** — That amount is outside the range this system can hold.
- **ar** — المبلغ خارج النطاق الذي يستطيع النظام تمثيله.

### `payroll.approved`

- **en** — Payroll run {id} has been approved and cannot be changed.
- **ar** — تم اعتماد مسيرة الرواتب {id} ولا يمكن تعديلها.

### `payroll.database`

- **en** — Payroll could not be read. Try again.
- **ar** — تعذّرت قراءة الرواتب. أعد المحاولة.

### `payroll.no_such_run`

- **en** — There is no payroll run {id}.
- **ar** — لا توجد مسيرة رواتب {id}.

### `payroll.nobody_to_pay`

- **en** — A payroll run needs somebody to pay.
- **ar** — مسيرة الرواتب تحتاج إلى من تُصرف له.

### `payroll.not_a_period`

- **en** — {period} is not a month. Use YYYY-MM.
- **ar** — {period} ليس شهرًا. استخدم صيغة YYYY-MM.

### `payroll.not_payable`

- **en** — {id} is not on the books, or has no salary recorded.
- **ar** — {id} ليس على رأس العمل، أو لا يوجد راتب مسجَّل له.


## `pos`

### `pos.amount_out_of_range`

- **en** — That amount is outside the range this system can hold.
- **ar** — هذا المبلغ خارج النطاق الذي يستطيع النظام حفظه.

### `pos.closed`

- **en** — Shift {id} has been closed and cannot take any more.
- **ar** — الوردية {id} أُغلقت ولا يمكنها استقبال المزيد.

### `pos.no_such_shift`

- **en** — There is no shift {id}.
- **ar** — لا توجد وردية {id}.

### `pos.not_a_float`

- **en** — An opening float cannot be negative.
- **ar** — رصيد الافتتاح لا يمكن أن يكون سالبًا.

### `pos.not_an_amount`

- **en** — An amount here must be more than nothing.
- **ar** — المبلغ هنا يجب أن يكون أكثر من صفر.

### `pos.nothing_sold`

- **en** — A sale needs at least one line on it.
- **ar** — البيع يحتاج إلى بند واحد على الأقل.

### `pos.tenders_do_not_match`

- **en** — The tenders come to {tendered} and the sale is {total}. A till sale is paid in full at the counter: less would leave a balance owing, and change handed back is not recorded.
- **ar** — مجموع المدفوعات {tendered} والبيع {total}. بيع الكاشير يُسدَّد بالكامل عند الصندوق: الأقل يترك رصيدًا مستحقًا، والباقي المعاد لا يُسجَّل.

### `pos.unknown_method`

- **en** — {method} is not a way money arrives. Use cash, card or transfer.
- **ar** — {method} ليست طريقة استلام. استخدم cash أو card أو transfer.


## `prepaid`

### `prepaid.already_frozen`

- **en** — Subscription {id} is already frozen.
- **ar** — الاشتراك {id} مجمّد بالفعل.

### `prepaid.amount_out_of_range`

- **en** — That amount is too large to record.
- **ar** — هذا المبلغ أكبر من أن يُسجَّل.

### `prepaid.cancelled`

- **en** — Subscription {id} has been cancelled.
- **ar** — تم إلغاء الاشتراك {id}.

### `prepaid.free_grant_with_value`

- **en** — Nobody paid for this, so it carries no value. A coupon is a discount, not a balance.
- **ar** — لم يدفع أحد مقابل هذا، فلا قيمة له. القسيمة خصم وليست رصيدًا.

### `prepaid.lapsed`

- **en** — {id} expired on {on}.
- **ar** — انتهت صلاحية {id} في {on}.

### `prepaid.no_scheme`

- **en** — No loyalty scheme has been configured, so there is nothing to earn against.
- **ar** — لم يتم إعداد برنامج ولاء، فلا شيء يُكتسب مقابله.

### `prepaid.no_such_card`

- **en** — There is no card {id}.
- **ar** — لا توجد بطاقة {id}.

### `prepaid.no_such_customer`

- **en** — There is no customer {customer}.
- **ar** — لا يوجد عميل {customer}.

### `prepaid.no_such_entitlement`

- **en** — There is no package or deposit {id}.
- **ar** — لا توجد باقة أو عربون باسم {id}.

### `prepaid.no_such_subscription`

- **en** — There is no subscription {id}.
- **ar** — لا يوجد اشتراك {id}.

### `prepaid.not_a_term`

- **en** — A term must end after it starts.
- **ar** — يجب أن تنتهي المدة بعد بدايتها.

### `prepaid.not_a_value`

- **en** — An amount here must be more than nothing.
- **ar** — يجب أن يكون المبلغ هنا أكبر من صفر.

### `prepaid.not_frozen`

- **en** — Subscription {id} is not frozen.
- **ar** — الاشتراك {id} غير مجمّد.

### `prepaid.not_live`

- **en** — {id} is finished and cannot be used again.
- **ar** — انتهى {id} ولا يمكن استخدامه مرة أخرى.

### `prepaid.nothing_left`

- **en** — Only {left} is left on {id}, and {wanted} was asked for.
- **ar** — لم يتبق سوى {left} في {id}، والمطلوب {wanted}.

### `prepaid.open_value`

- **en** — An amount must either count uses or name what it is held against. A card spendable on anything is not supported.
- **ar** — المبلغ يجب أن يحدّد عدد الاستخدامات أو ما هو محجوز مقابله. البطاقة القابلة للصرف على أي شيء غير مدعومة.

### `prepaid.term_not_over`

- **en** — The current term of {id} runs until {until} and cannot be renewed yet.
- **ar** — تستمر مدة {id} الحالية حتى {until} ولا يمكن تجديدها بعد.

### `prepaid.unknown_mechanic`

- **en** — {mechanic} is not a way a card counts.
- **ar** — {mechanic} ليست طريقة عدّ لبطاقة.

### `prepaid.unknown_reason`

- **en** — {value} is not a way something is granted.
- **ar** — {value} ليست طريقة يُمنح بها شيء.

### `prepaid.wrong_currency`

- **en** — Card {id} holds a balance in another currency than the scheme.
- **ar** — البطاقة {id} تحمل رصيدًا بعملة غير عملة البرنامج.


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


## `recurrence`

### `recurrence.backwards_dates`

- **en** — The last day these hours apply is before the first.
- **ar** — آخر يوم تسري فيه هذه الساعات يسبق أولها.

### `recurrence.not_a_day_of_the_month`

- **en** — {value} is not a day of any month.
- **ar** — {value} ليس يومًا في أي شهر.

### `recurrence.not_a_month`

- **en** — {value} is not a month.
- **ar** — {value} ليس شهرًا.

### `recurrence.not_a_time_of_day`

- **en** — A time of day is minutes past midnight, from 0 to {most}.
- **ar** — وقت اليوم هو عدد الدقائق بعد منتصف الليل، من 0 إلى {most}.

### `recurrence.not_a_weekday`

- **en** — {value} is not a weekday. Monday is 1 and Sunday is 7.
- **ar** — {value} ليس يومًا من أيام الأسبوع. الاثنين هو 1 والأحد هو 7.

### `recurrence.not_a_window`

- **en** — Opening hours must close after they open. A window that runs past midnight is two windows.
- **ar** — يجب أن تنتهي ساعات العمل بعد بدايتها. النافذة التي تتجاوز منتصف الليل هي نافذتان.

### `recurrence.not_an_offset`

- **en** — A timezone offset is minutes from UTC, between -{limit} and {limit}.
- **ar** — فرق التوقيت هو عدد الدقائق عن التوقيت العالمي، بين -{limit} و {limit}.


## `reports`

### `reports.backwards`

- **en** — A report ends after it starts. {from} is later than {until}.
- **ar** — ينتهي التقرير بعد أن يبدأ. {from} بعد {until}.

### `reports.database`

- **en** — That could not be read. Try again.
- **ar** — تعذّرت القراءة. أعد المحاولة.

### `reports.does_not_reconcile`

- **en** — These figures do not agree with the books: {n} discrepancies. Nothing is shown until they are resolved.
- **ar** — هذه الأرقام لا تطابق الدفاتر: {n} فرق. لا يُعرض شيء حتى تتم تسويتها.

### `reports.not_a_period`

- **en** — {period} is not a month. Write it as 2026-05.
- **ar** — {period} ليس شهرًا. اكتبه هكذا: 2026-05.

### `reports.range_too_long`

- **en** — A report may cover at most {n} months.
- **ar** — لا يغطي التقرير أكثر من {n} شهر.


## `request`

### `request.certificate_key_mismatch`

- **en** — That certificate is not for the private key held for this business. Every invoice signed with it would be rejected, so it has not been stored.
- **ar** — هذه الشهادة ليست للمفتاح الخاص المحفوظ لهذه المنشأة. كل فاتورة تُوقَّع بها سترفض، لذلك لم تُحفظ.

### `request.compliance_refused`

- **en** — ZATCA refused {failed} of the {submitted} compliance documents this system generated, so it cannot go live. This is a fault in this software rather than in your request: {reason}
- **ar** — رفضت هيئة الزكاة والضريبة والجمارك {failed} من أصل {submitted} من مستندات الفحص التي أنشأها النظام، فتعذّر التفعيل. الخلل في النظام وليس في طلبك: {reason}

### `request.csid_not_issued`

- **en** — ZATCA did not issue a certificate ({disposition}): {detail}
- **ar** — لم تُصدر هيئة الزكاة والضريبة والجمارك شهادة ({disposition}): {detail}

### `request.empty_period`

- **en** — A period must end after it starts. `until` is exclusive.
- **ar** — يجب أن تنتهي الفترة بعد بدايتها. تاريخ الانتهاء غير شامل.

### `request.invalid_cursor`

- **en** — {after} is not a page cursor from this API. Pass back the `next` value from a previous response, or leave it out to start from the beginning.
- **ar** — {after} ليس مؤشر صفحة من هذه الواجهة. أعد إرسال قيمة `next` من الاستجابة السابقة، أو اتركه فارغًا للبدء من الأول.

### `request.invalid_id`

- **en** — {id} cannot be used as an identifier.
- **ar** — لا يمكن استخدام {id} كمعرّف.

### `request.invalid_query`

- **en** — The query string could not be read: {reason}
- **ar** — تعذّرت قراءة معطيات الاستعلام: {reason}

### `request.malformed_body`

- **en** — The request body could not be read: {reason}
- **ar** — تعذّرت قراءة محتوى الطلب: {reason}

### `request.missing_idempotency_key`

- **en** — This request needs an `Idempotency-Key` header holding a UUID. It is the identity the record is created under, so it must be generated per request and never reused for a different one.
- **ar** — هذا الطلب يحتاج ترويسة `Idempotency-Key` تحتوي على UUID. هي الهوية التي يُنشأ بها السجل، فيجب توليدها لكل طلب وعدم إعادة استخدامها لطلب مختلف.

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

### `request.module_requires_one_of`

- **en** — The {module} module needs at least one of: {required}. Add one to the list.
- **ar** — تحتاج وحدة {module} إلى واحدة على الأقل من: {required}. أضف واحدة إلى القائمة.

### `request.no_sealing_key`

- **en** — This deployment has no sealing key, so a private key cannot be stored safely. Set SEALING_KEY and try again.
- **ar** — لا يوجد مفتاح تشفير مُهيّأ في هذا النظام، فلا يمكن حفظ المفتاح الخاص بأمان. اضبط SEALING_KEY ثم أعد المحاولة.

### `request.no_such_bill`

- **en** — There is no bill {bill}.
- **ar** — لا توجد فاتورة مورّد {bill}.

### `request.no_such_invoice`

- **en** — There is no invoice {invoice}.
- **ar** — لا توجد فاتورة {invoice}.

### `request.not_an_otp`

- **en** — That is not a Fatoora OTP. It is six digits, generated in the ZATCA portal, and it lasts about an hour.
- **ar** — هذا ليس رمز تحقق من بوابة فاتورة. الرمز ستة أرقام يُنشأ من البوابة وتنتهي صلاحيته خلال ساعة تقريبًا.

### `request.not_caught_up`

- **en** — Still catching up ({behind} to go). Please try again in a moment.
- **ar** — لا يزال التحديث جاريًا (متبقٍ {behind}). يُرجى المحاولة بعد لحظات.

### `request.onboarding_not_yet`

- **en** — This business has no {stage} certificate yet.
- **ar** — لا توجد شهادة {stage} لهذه المنشأة بعد.

### `request.password_too_short`

- **en** — A password needs at least {n} characters.
- **ar** — تحتاج كلمة المرور إلى {n} حرف على الأقل.

### `request.too_many_requests`

- **en** — Too many requests. Try again in {seconds} seconds.
- **ar** — طلبات كثيرة جدًا. أعد المحاولة بعد {seconds} ثانية.

### `request.unknown_account_kind`

- **en** — {kind} is not an account type. Use asset, liability, equity, revenue or expense.
- **ar** — {kind} ليس نوع حساب. استخدم أصل أو التزام أو حقوق ملكية أو إيراد أو مصروف.

### `request.unknown_chart`

- **en** — There is no chart of accounts called {chart}.
- **ar** — لا يوجد دليل حسابات باسم {chart}.

### `request.unknown_currency`

- **en** — {currency} is not a currency code. Use three letters, like SAR.
- **ar** — {currency} ليس رمز عملة. استخدم ثلاثة أحرف، مثل SAR.

### `request.unknown_id_scheme`

- **en** — {scheme} is not an identification scheme. Use crn, mom, mls, sag, number700 or other.
- **ar** — {scheme} ليس نوع سجل. استخدم crn أو mom أو mls أو sag أو number700 أو other.

### `request.unknown_module`

- **en** — There is no module called {module}.
- **ar** — لا توجد وحدة باسم {module}.

### `request.unknown_onboarding_stage`

- **en** — {stage} is not an onboarding stage. Use compliance or production.
- **ar** — {stage} ليست مرحلة تسجيل. استخدم compliance أو production.

### `request.unknown_role`

- **en** — {role} is not a role. Use owner, accountant, clerk or viewer.
- **ar** — {role} ليس دورًا. استخدم owner أو accountant أو clerk أو viewer.

### `request.unknown_vat_category`

- **en** — {vat} is not a VAT treatment. Use standard, zero or exempt.
- **ar** — {vat} ليست معاملة ضريبية. استخدم standard أو zero أو exempt.

### `request.unknown_zatca_environment`

- **en** — {environment} is not a ZATCA environment. Use sandbox, simulation or production.
- **ar** — {environment} ليست بيئة لدى هيئة الزكاة والضريبة والجمارك. استخدم sandbox أو simulation أو production.

### `request.unreadable_certificate`

- **en** — That is not a certificate this system can read: {reason}.
- **ar** — تعذّرت قراءة الشهادة: {reason}.

### `request.unsupported_media_type`

- **en** — This endpoint takes `Content-Type: application/json`.
- **ar** — يقبل هذا المسار `Content-Type: application/json` فقط.

### `request.unusable_unit`

- **en** — That unit cannot go in a certificate request: {reason}.
- **ar** — لا يمكن استخدام بيانات الوحدة في طلب الشهادة: {reason}.

### `request.unusable_vat_rate`

- **en** — {rate} is not a usable VAT rate. Give it in basis points, between 0 and 10000 — 1500 is 15%.
- **ar** — {rate} ليست نسبة ضريبة صالحة. أدخلها بنقاط الأساس بين 0 و10000 — القيمة 1500 تعني 15%.

### `request.zatca_unreachable`

- **en** — ZATCA could not be reached while {step}: {reason}. Nothing beyond the last completed step was changed.
- **ar** — تعذّر الوصول إلى هيئة الزكاة والضريبة والجمارك أثناء {step}: {reason}. لم يتغيّر شيء بعد آخر خطوة اكتملت.


## `sales`

### `sales.already_cancelled`

- **en** — That invoice was already cancelled by credit note {by}.
- **ar** — تم إلغاء هذه الفاتورة بالفعل بإشعار دائن {by}.

### `sales.amount_out_of_range`

- **en** — That amount is too large to record.
- **ar** — هذا المبلغ أكبر من أن يُسجَّل.

### `sales.discount_too_large`

- **en** — A discount cannot be larger than what it is taken off.
- **ar** — لا يمكن أن يتجاوز الخصم قيمة ما يُخصم منه.

### `sales.discount_without_a_band`

- **en** — Nothing on this invoice is taxed the way that discount is. Discounting at a rate the invoice does not charge would reclaim tax that was never charged.
- **ar** — لا يوجد بند في هذه الفاتورة بنفس المعاملة الضريبية للخصم. الخصم بمعاملة لا تتضمنها الفاتورة يسترد ضريبة لم تُحتسب أصلًا.

### `sales.has_payments`

- **en** — Invoice {invoice} has payments against it. Refund them before crediting it.
- **ar** — توجد دفعات على الفاتورة {invoice}. أعِد المبالغ قبل إصدار إشعار دائن.

### `sales.invalid_reference`

- **en** — {reference} cannot be used as a reference. Use letters, digits, and . - _ only.
- **ar** — لا يمكن استخدام {reference} كمرجع. استخدم الحروف والأرقام و. - _ فقط.

### `sales.mixed_currencies`

- **en** — Every line of an invoice must be in the same currency.
- **ar** — يجب أن تكون جميع سطور الفاتورة بالعملة نفسها.

### `sales.no_such_customer`

- **en** — There is no customer {customer} to issue this to. Record them first, or leave the customer reference out.
- **ar** — لا يوجد عميل {customer} لإصدار الفاتورة له. سجّله أولًا أو اترك مرجع العميل فارغًا.

### `sales.not_a_discount`

- **en** — A discount is the amount taken off, so it is positive. A negative one is a charge.
- **ar** — الخصم هو المبلغ المحسوم، لذا يكون موجبًا. القيمة السالبة تُعد رسمًا إضافيًا.

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

### `sales.overrefund`

- **en** — The business is holding only {held} against this invoice and the refund is {offered}. Handing back more than was taken is a decision somebody has to make, not a negative balance.
- **ar** — المحتفظ به مقابل هذه الفاتورة {held} والمبلغ المسترد {offered}. إعادة أكثر مما استُلم قرار يتخذه شخص، لا رصيد سالب.

### `sales.payment_currency`

- **en** — This invoice is in {expected}, but the payment is in {found}.
- **ar** — هذه الفاتورة بعملة {expected}، لكن الدفعة بعملة {found}.


## `signups`

### `signups.not_valid`

- **en** — That confirmation link is not valid. It may have expired, or been used already.
- **ar** — رابط التأكيد غير صالح. ربما انتهت صلاحيته أو استُخدم من قبل.

### `signups.too_soon`

- **en** — A confirmation is already on its way. Try again in {seconds} seconds.
- **ar** — رسالة التأكيد في طريقها إليك. أعد المحاولة بعد {seconds} ثانية.


## `system`

### `system.internal_error`

- **en** — Something went wrong on our side. The problem has been recorded.
- **ar** — حدث خطأ لدينا. تم تسجيل المشكلة.

### `system.overloaded`

- **en** — The system is busy right now. Please try again in a moment.
- **ar** — النظام مشغول حالياً. يُرجى المحاولة مرة أخرى بعد قليل.


## `tax_sa`

### `tax_sa.already_filed`

- **en** — The period {period} was filed on {on}. Correcting a filed return is an amendment, not a second filing.
- **ar** — قُدِّم إقرار الفترة {period} بتاريخ {on}. تصحيح إقرار مُقدَّم يكون بتعديل وليس بإقرار ثانٍ.

### `tax_sa.empty_period`

- **en** — A period must end after it starts. `until` is exclusive.
- **ar** — يجب أن تنتهي الفترة بعد بدايتها. تاريخ الانتهاء غير شامل.

### `tax_sa.invalid_document`

- **en** — {document} cannot be used as a document identifier.
- **ar** — لا يمكن استخدام {document} كمعرّف مستند.

### `tax_sa.invalid_period`

- **en** — {period} cannot be used as a period identifier.
- **ar** — لا يمكن استخدام {period} كمعرّف فترة.

### `tax_sa.invalid_registration`

- **en** — That ZATCA registration cannot be used: {reason}. It is checked here because a standard invoice cannot be given to a buyer until ZATCA has cleared it.
- **ar** — لا يمكن استخدام بيانات التسجيل لدى هيئة الزكاة والضريبة والجمارك: {reason}. تُفحص هنا لأن الفاتورة الضريبية لا تُسلَّم للمشتري قبل اعتمادها.

### `tax_sa.no_such_document`

- **en** — There is no ZATCA document numbered {document}.
- **ar** — لا يوجد مستند برقم {document}.

### `tax_sa.not_registered`

- **en** — This business has no ZATCA registration yet, so no invoice can be cleared or reported. Register one first.
- **ar** — لا يوجد تسجيل لدى هيئة الزكاة والضريبة والجمارك لهذه المنشأة بعد، فلا يمكن اعتماد أي فاتورة أو الإبلاغ عنها. سجِّل البيانات أولًا.

